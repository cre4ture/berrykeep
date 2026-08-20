use super::*;
use aes_gcm_siv::aead::Aead;
use aes_gcm_siv::{Aes256GcmSiv, KeyInit, Nonce};
use axum_server::Handle;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use rcgen::BasicConstraints;
use sha2::Sha256;
use std::net::Ipv4Addr;
use tokio::sync::mpsc::{self, OwnedPermit};

const LEGACY_SETUP_STATE_VERSION: u32 = 1;
const LEGACY_SETUP_STATE_VERSION_2: u32 = 2;
const SETUP_STATE_VERSION: u32 = 3;
const MANAGED_SIGNER_BACKUP_VERSION: u32 = 1;
const MANAGED_RENDEZVOUS_FAILOVER_VERSION: u32 = 1;
const MANAGED_SIGNER_BACKUP_SALT_LEN: usize = 16;
const MANAGED_SIGNER_BACKUP_NONCE_LEN: usize = 12;
const MANAGED_SIGNER_BACKUP_KEY_LEN: usize = 32;
const MANAGED_SIGNER_BACKUP_PBKDF2_ROUNDS: u32 = 600_000;
const SETUP_RUNTIME_TRANSITION_DELAY_MS: u64 = 100;
const MANAGED_INTERNAL_CA_CERT_PATH: &str = "managed/runtime/internal/cluster-ca.pem";
const MANAGED_INTERNAL_CERT_PATH: &str = "managed/runtime/internal/node.pem";
const MANAGED_INTERNAL_KEY_PATH: &str = "managed/runtime/internal/node.key";
const MANAGED_PUBLIC_CERT_PATH: &str = "managed/runtime/public/public.pem";
const MANAGED_PUBLIC_KEY_PATH: &str = "managed/runtime/public/public.key";
const MANAGED_PUBLIC_CA_CERT_PATH: &str = "managed/runtime/public/public-ca.pem";

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum StartupMode {
    Runtime(ServerNodeConfig),
    Setup(SetupBootstrapConfig),
}

#[derive(Debug, Clone)]
pub(crate) struct SetupBootstrapConfig {
    data_dir: PathBuf,
    bind_addr: SocketAddr,
    state_path: PathBuf,
    bootstrap_cert_path: PathBuf,
    bootstrap_key_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SetupLifecycleState {
    Uninitialized,
    PendingJoin,
    Recovery,
    Online,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SetupMetadataBackend {
    Sqlite,
    Turso,
}

impl SetupMetadataBackend {
    fn into_runtime_kind(self) -> Result<MetadataBackendKind> {
        match self {
            Self::Sqlite => Ok(MetadataBackendKind::Sqlite),
            Self::Turso => {
                #[cfg(feature = "turso-metadata")]
                {
                    Ok(MetadataBackendKind::Turso)
                }
                #[cfg(not(feature = "turso-metadata"))]
                {
                    bail!("the Turso metadata backend is unavailable in this server-node build")
                }
            }
        }
    }
}

impl From<MetadataBackendKind> for SetupMetadataBackend {
    fn from(value: MetadataBackendKind) -> Self {
        match value {
            MetadataBackendKind::Sqlite => Self::Sqlite,
            #[cfg(feature = "turso-metadata")]
            MetadataBackendKind::Turso => Self::Turso,
        }
    }
}

fn default_setup_metadata_backend() -> SetupMetadataBackend {
    #[cfg(feature = "turso-metadata")]
    {
        SetupMetadataBackend::Turso
    }
    #[cfg(not(feature = "turso-metadata"))]
    {
        SetupMetadataBackend::Sqlite
    }
}

fn available_setup_metadata_backends() -> Vec<SetupMetadataBackend> {
    #[cfg(feature = "turso-metadata")]
    {
        vec![SetupMetadataBackend::Sqlite, SetupMetadataBackend::Turso]
    }
    #[cfg(not(feature = "turso-metadata"))]
    {
        vec![SetupMetadataBackend::Sqlite]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManagedRecoveryReasonCode {
    EnrollmentPackageMissing,
    EnrollmentPackageInvalid,
    EnrollmentIdentityMismatch,
    CertificateMaterialMissing,
    CertificateExpired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedRecoveryReason {
    code: ManagedRecoveryReasonCode,
    detected_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ManagedRecoveryReason {
    fn new(code: ManagedRecoveryReasonCode, detail: impl Into<Option<String>>) -> Self {
        Self {
            code,
            detected_at_unix: unix_ts(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManagedSetupState {
    version: u32,
    state: SetupLifecycleState,
    updated_at_unix: u64,
    cluster_id: Option<ClusterId>,
    node_id: Option<NodeId>,
    /// Read-only compatibility input for setup-state v1. Version 2 and later derive the enrollment
    /// path from the setup data directory and never serialize this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_node_enrollment_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_data_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_reason: Option<ManagedRecoveryReason>,
    /// Absent only in setup-state v1/v2. Migration imports the legacy environment selection once,
    /// falling back to SQLite, and version 3 then treats this persisted value as authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_backend: Option<SetupMetadataBackend>,
    pub(crate) admin_password_hash: Option<String>,
    managed_rendezvous_bind_addr: Option<String>,
    managed_rendezvous_public_url: Option<String>,
    pending_join_request: Option<NodeJoinRequest>,
}

impl Default for ManagedSetupState {
    fn default() -> Self {
        Self {
            version: SETUP_STATE_VERSION,
            state: SetupLifecycleState::Uninitialized,
            updated_at_unix: unix_ts(),
            cluster_id: None,
            node_id: None,
            runtime_node_enrollment_path: None,
            runtime_data_dir: None,
            recovery_reason: None,
            metadata_backend: Some(default_setup_metadata_backend()),
            admin_password_hash: None,
            managed_rendezvous_bind_addr: None,
            managed_rendezvous_public_url: None,
            pending_join_request: None,
        }
    }
}

#[derive(Clone)]
struct SetupServerState {
    config: SetupBootstrapConfig,
    managed_state: Arc<Mutex<ManagedSetupState>>,
    completion_tx: mpsc::Sender<SetupCompletion>,
}

#[derive(Debug)]
struct SetupCompletion {
    config: ServerNodeConfig,
}

fn spawn_setup_runtime_transition(permit: OwnedPermit<SetupCompletion>, config: ServerNodeConfig) {
    tokio::spawn(async move {
        // Let the setup handler finish writing its JSON response before the
        // supervisor tears the bootstrap server down and swaps into runtime.
        tokio::time::sleep(Duration::from_millis(SETUP_RUNTIME_TRANSITION_DELAY_MS)).await;
        permit.send(SetupCompletion { config });
    });
}

struct SelfManagedClusterArtifacts {
    package: NodeEnrollmentPackage,
    ca_cert_pem: String,
    ca_key_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManagedSignerBackup {
    pub version: u32,
    pub cluster_id: ClusterId,
    pub source_node_id: NodeId,
    pub exported_at_unix: u64,
    pub pbkdf2_rounds: u32,
    pub salt_b64: String,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedSignerBackupPlaintext {
    cluster_id: ClusterId,
    source_node_id: NodeId,
    exported_at_unix: u64,
    ca_cert_pem: String,
    ca_key_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManagedRendezvousFailoverPackage {
    pub version: u32,
    pub cluster_id: ClusterId,
    pub source_node_id: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_node_id: Option<NodeId>,
    pub exported_at_unix: u64,
    pub public_url: String,
    #[serde(default)]
    pub deployment_target: ManagedRendezvousFailoverDeploymentTarget,
    #[serde(default)]
    pub includes_cluster_ca_cert: bool,
    pub pbkdf2_rounds: u32,
    pub salt_b64: String,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedRendezvousFailoverDeploymentTarget {
    #[default]
    EmbeddedNode,
    StandaloneService,
}

pub(crate) struct ManagedRendezvousFailoverExportParams<'a> {
    pub cluster_id: ClusterId,
    pub source_node_id: NodeId,
    pub target_node_id: Option<NodeId>,
    pub public_url: &'a str,
    pub deployment_target: ManagedRendezvousFailoverDeploymentTarget,
    pub client_ca_cert_pem: &'a str,
    pub cert_pem: &'a str,
    pub key_pem: &'a str,
    pub passphrase: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedRendezvousFailoverPlaintext {
    cluster_id: ClusterId,
    source_node_id: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_node_id: Option<NodeId>,
    exported_at_unix: u64,
    public_url: String,
    #[serde(default)]
    client_ca_cert_pem: Option<String>,
    cert_pem: String,
    key_pem: String,
}

#[derive(Debug, Serialize)]
struct SetupStatusResponse {
    state: SetupLifecycleState,
    data_dir: String,
    bind_addr: String,
    bootstrap_tls_cert_path: String,
    bootstrap_tls_fingerprint: Option<String>,
    cluster_id: Option<ClusterId>,
    node_id: Option<NodeId>,
    recovery_reason: Option<ManagedRecoveryReason>,
    metadata_backend: SetupMetadataBackend,
    available_metadata_backends: Vec<SetupMetadataBackend>,
    pending_join_request: Option<NodeJoinRequest>,
}

#[derive(Debug, Deserialize)]
struct SetupStartClusterRequest {
    admin_password: String,
    public_origin: String,
    #[serde(default)]
    metadata_backend: Option<SetupMetadataBackend>,
    /// Setup-time reliability-telemetry disclosure choice
    /// (`docs/server-node-hardware-reliability-telemetry-strategy.md` Section 4.4). The setup
    /// wizard's disclosure step always sends this explicitly, pre-checked `true`; it defaults to
    /// `true` here only so that opt-out remains the outcome for any non-UI caller that omits the
    /// field entirely, consistent with the rest of the opt-out model.
    #[serde(default = "default_setup_telemetry_enabled")]
    telemetry_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct SetupGenerateJoinRequest {
    public_origin: String,
}

#[derive(Debug, Deserialize)]
struct SetupImportEnrollmentRequest {
    // Initializes the joining node's local admin credential after the
    // enrollment package is imported. This is not verified against an
    // existing cluster member.
    admin_password: String,
    package_json: String,
    #[serde(default)]
    metadata_backend: Option<SetupMetadataBackend>,
    /// See `SetupStartClusterRequest::telemetry_enabled` (doc Section 4.4); the join flow shows
    /// the same disclosure step and defaults identically.
    #[serde(default = "default_setup_telemetry_enabled")]
    telemetry_enabled: bool,
}

fn default_setup_telemetry_enabled() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct SetupTransitionResponse {
    status: &'static str,
    cluster_id: ClusterId,
    node_id: NodeId,
    public_url: Option<String>,
    metadata_backend: SetupMetadataBackend,
    restart_required: bool,
}

pub(crate) fn load_startup_mode_from_env() -> Result<StartupMode> {
    let explicit_runtime_env_vars = explicit_runtime_env_vars_present();
    if !explicit_runtime_env_vars.is_empty() {
        tracing::info!(
            startup_mode = "runtime",
            startup_reason = "explicit_runtime_environment",
            env_vars = %explicit_runtime_env_vars.join(","),
            "server node startup selected normal runtime mode"
        );
        return Ok(StartupMode::Runtime(ServerNodeConfig::from_env()?));
    }

    load_managed_startup_mode(default_setup_bootstrap_config()?)
}

pub(crate) fn load_managed_startup_mode(config: SetupBootstrapConfig) -> Result<StartupMode> {
    tracing::info!(
        data_dir = %config.data_dir.display(),
        state_path = %config.state_path.display(),
        bind_addr = %config.bind_addr,
        "server node checking managed startup state"
    );

    let Some(mut managed_state) = read_managed_setup_state(&config.state_path).map_err(|err| {
        tracing::error!(
            state_path = %config.state_path.display(),
            error = %err,
            "server node failed to read managed startup state"
        );
        err
    })?
    else {
        tracing::info!(
            startup_mode = "bootstrap_setup",
            startup_reason = "managed_setup_state_missing",
            state_path = %config.state_path.display(),
            data_dir = %config.data_dir.display(),
            "server node startup selected bootstrap setup mode"
        );
        return Ok(StartupMode::Setup(config));
    };

    let metadata_backend_migrated =
        migrate_managed_setup_metadata_backend(&config.state_path, &mut managed_state)
            .context("failed migrating managed setup metadata backend")?;
    let metadata_backend = managed_state
        .metadata_backend
        .context("managed setup state is missing metadata_backend")?
        .into_runtime_kind()?;

    if !matches!(
        managed_state.state,
        SetupLifecycleState::Online | SetupLifecycleState::Recovery
    ) {
        tracing::info!(
            startup_mode = "bootstrap_setup",
            startup_reason = "managed_setup_state_not_online",
            managed_state = ?managed_state.state,
            state_path = %config.state_path.display(),
            cluster_id = ?managed_state.cluster_id,
            node_id = ?managed_state.node_id,
            "server node startup selected bootstrap setup mode"
        );
        return Ok(StartupMode::Setup(config));
    }

    let source_enrollment_path = match find_managed_enrollment_path(&config, &managed_state) {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(
                startup_mode = "bootstrap_setup",
                startup_reason = "runtime_node_enrollment_path_missing_or_invalid",
                state_path = %config.state_path.display(),
                error = %err,
                cluster_id = ?managed_state.cluster_id,
                node_id = ?managed_state.node_id,
                "server node managed state has no usable runtime enrollment package"
            );
            transition_managed_setup_state_to_recovery(
                &config.state_path,
                &mut managed_state,
                ManagedRecoveryReason::new(
                    ManagedRecoveryReasonCode::EnrollmentPackageMissing,
                    Some(err.to_string()),
                ),
            )?;
            return Ok(StartupMode::Setup(config));
        }
    };
    if !source_enrollment_path.exists() {
        tracing::warn!(
            startup_mode = "bootstrap_setup",
            startup_reason = "runtime_node_enrollment_file_missing",
            state_path = %config.state_path.display(),
            resolved_enrollment_path = %source_enrollment_path.display(),
            cluster_id = ?managed_state.cluster_id,
            node_id = ?managed_state.node_id,
            "server node managed state has no runtime enrollment file"
        );
        transition_managed_setup_state_to_recovery(
            &config.state_path,
            &mut managed_state,
            ManagedRecoveryReason::new(
                ManagedRecoveryReasonCode::EnrollmentPackageMissing,
                Some(format!(
                    "runtime enrollment file {} does not exist",
                    source_enrollment_path.display()
                )),
            ),
        )?;
        return Ok(StartupMode::Setup(config));
    }

    let original_package = match NodeEnrollmentPackage::from_path(&source_enrollment_path) {
        Ok(package) => package,
        Err(err) => {
            tracing::error!(
                startup_mode = "bootstrap_setup",
                startup_reason = "runtime_node_enrollment_load_failed",
                state_path = %config.state_path.display(),
                resolved_enrollment_path = %source_enrollment_path.display(),
                error = %err,
                "server node failed to load managed runtime enrollment"
            );
            transition_managed_setup_state_to_recovery(
                &config.state_path,
                &mut managed_state,
                ManagedRecoveryReason::new(
                    ManagedRecoveryReasonCode::EnrollmentPackageInvalid,
                    Some(err.to_string()),
                ),
            )?;
            return Ok(StartupMode::Setup(config));
        }
    };
    let runtime_data_dir = match managed_runtime_data_dir(&managed_state, &original_package) {
        Ok(path) => path,
        Err(err) => {
            transition_managed_setup_state_to_recovery(
                &config.state_path,
                &mut managed_state,
                ManagedRecoveryReason::new(
                    ManagedRecoveryReasonCode::EnrollmentPackageInvalid,
                    Some(err.to_string()),
                ),
            )?;
            return Ok(StartupMode::Setup(config));
        }
    };
    let managed_package =
        match canonical_managed_node_enrollment(original_package.clone(), &runtime_data_dir) {
            Ok(package) => package,
            Err(err) => {
                transition_managed_setup_state_to_recovery(
                    &config.state_path,
                    &mut managed_state,
                    ManagedRecoveryReason::new(
                        ManagedRecoveryReasonCode::EnrollmentPackageInvalid,
                        Some(err.to_string()),
                    ),
                )?;
                return Ok(StartupMode::Setup(config));
            }
        };
    let canonical_enrollment_path = runtime_node_enrollment_path(&config.data_dir);
    let mut runtime = match ServerNodeConfig::from_enrollment_with_metadata_backend(
        managed_package.clone(),
        metadata_backend,
    ) {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(
                startup_mode = "bootstrap_setup",
                startup_reason = "runtime_node_enrollment_materialization_failed",
                state_path = %config.state_path.display(),
                resolved_enrollment_path = %source_enrollment_path.display(),
                runtime_data_dir = %runtime_data_dir.display(),
                error = %err,
                "server node failed to materialize managed runtime enrollment"
            );
            transition_managed_setup_state_to_recovery(
                &config.state_path,
                &mut managed_state,
                ManagedRecoveryReason::new(
                    ManagedRecoveryReasonCode::EnrollmentPackageInvalid,
                    Some(err.to_string()),
                ),
            )?;
            return Ok(StartupMode::Setup(config));
        }
    };
    runtime.node_enrollment_path = Some(canonical_enrollment_path.clone());
    runtime.node_enrollment_auto_renew_enabled =
        parse_enrollment_auto_renew_enabled(default_node_enrollment_auto_renew_enabled(&runtime));
    runtime.node_enrollment_auto_renew_check_secs = node_enrollment_auto_renew_check_secs();

    if managed_state
        .cluster_id
        .is_some_and(|cluster_id| cluster_id != runtime.cluster_id)
        || managed_state
            .node_id
            .is_some_and(|node_id| node_id != runtime.node_id)
    {
        let identity_mismatch_detail = format!(
            "runtime enrollment identity {}/{} does not match managed state {}/{}",
            runtime.cluster_id,
            runtime.node_id,
            managed_state
                .cluster_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unset".to_string()),
            managed_state
                .node_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unset".to_string())
        );
        transition_managed_setup_state_to_recovery(
            &config.state_path,
            &mut managed_state,
            ManagedRecoveryReason::new(
                ManagedRecoveryReasonCode::EnrollmentIdentityMismatch,
                Some(identity_mismatch_detail),
            ),
        )?;
        return Ok(StartupMode::Setup(config));
    }

    apply_managed_signer_paths(&config.data_dir, &mut runtime);
    apply_managed_rendezvous_config(&config.data_dir, &managed_state, &mut runtime);

    if original_package != managed_package || source_enrollment_path != canonical_enrollment_path {
        managed_package
            .write_to_path(&canonical_enrollment_path)
            .context("failed persisting canonical managed runtime enrollment")?;
    }

    let recovered = managed_state.state == SetupLifecycleState::Recovery;
    let runtime_data_dir_string = runtime_data_dir.display().to_string();
    let setup_model_changed = managed_state.version != SETUP_STATE_VERSION
        || managed_state.cluster_id != Some(runtime.cluster_id)
        || managed_state.node_id != Some(runtime.node_id)
        || managed_state.runtime_node_enrollment_path.is_some()
        || managed_state.runtime_data_dir.as_deref() != Some(runtime_data_dir_string.as_str());
    let model_migrated = metadata_backend_migrated || setup_model_changed;
    managed_state.version = SETUP_STATE_VERSION;
    managed_state.cluster_id = Some(runtime.cluster_id);
    managed_state.node_id = Some(runtime.node_id);
    managed_state.runtime_node_enrollment_path = None;
    managed_state.runtime_data_dir = Some(runtime_data_dir_string);
    if setup_model_changed {
        managed_state.updated_at_unix = unix_ts();
        write_managed_setup_state(&config.state_path, &managed_state)?;
    }

    if let Some(reason_code) = runtime_enrollment_recovery_reason(&runtime) {
        tracing::warn!(
            startup_mode = "bootstrap_setup",
            startup_reason = "runtime_node_enrollment_requires_rejoin",
            state_path = %config.state_path.display(),
            resolved_enrollment_path = %source_enrollment_path.display(),
            cluster_id = %runtime.cluster_id,
            node_id = %runtime.node_id,
            "server node startup selected setup recovery mode"
        );
        transition_managed_setup_state_to_recovery(
            &config.state_path,
            &mut managed_state,
            ManagedRecoveryReason::new(reason_code, None),
        )?;
        return Ok(StartupMode::Setup(config));
    }

    let lifecycle_changed = managed_state.state != SetupLifecycleState::Online
        || managed_state.recovery_reason.is_some();
    managed_state.state = SetupLifecycleState::Online;
    managed_state.recovery_reason = None;
    if lifecycle_changed {
        managed_state.updated_at_unix = unix_ts();
        write_managed_setup_state(&config.state_path, &managed_state)?;
    }

    runtime.admin_password_hash = managed_state.admin_password_hash.clone();
    tracing::info!(
        startup_mode = "runtime",
        startup_reason = if recovered {
            "managed_setup_recovered"
        } else if model_migrated {
            "managed_setup_state_migrated"
        } else {
            "managed_setup_state_online"
        },
        state_path = %config.state_path.display(),
        enrollment_path = %runtime_node_enrollment_relative_path().display(),
        resolved_enrollment_path = %canonical_enrollment_path.display(),
        runtime_data_dir = %runtime_data_dir.display(),
        cluster_id = %runtime.cluster_id,
        node_id = %runtime.node_id,
        "server node startup selected normal runtime mode"
    );
    Ok(StartupMode::Runtime(runtime))
}

pub(crate) async fn run_setup_mode(
    config: SetupBootstrapConfig,
    log_buffer: Arc<LogBuffer>,
    runtime_log_control: RuntimeLogControl,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let initial_state =
        ensure_managed_setup_state(&config.state_path).context("failed preparing setup state")?;
    let tls_config = ensure_bootstrap_tls_config(&config).await?;
    let (completion_tx, mut completion_rx) = mpsc::channel::<SetupCompletion>(1);
    let app_state = SetupServerState {
        config: config.clone(),
        managed_state: Arc::new(Mutex::new(initial_state)),
        completion_tx,
    };

    let app = Router::new()
        .route("/", get(ui::index))
        .route("/health", get(setup_health))
        .route("/ironmesh-favicon.svg", get(ui::favicon))
        .route("/ui/app.css", get(ui::app_css))
        .route("/ui/app.js", get(ui::app_js))
        .route("/setup/status", get(get_setup_status))
        .route("/setup/start-cluster", post(start_new_cluster))
        .route("/setup/join/request", post(generate_join_request))
        .route("/setup/join/import", post(import_node_enrollment_package))
        .with_state(app_state);

    let handle = Handle::new();
    let server = axum_server::bind_rustls(config.bind_addr, tls_config)
        .handle(handle.clone())
        .serve(app.into_make_service());
    let server_task = tokio::spawn(server);

    info!(
        bind_addr = %config.bind_addr,
        data_dir = %config.data_dir.display(),
        "server node bootstrap setup listener"
    );

    let completion = tokio::select! {
        completion = completion_rx.recv() => completion,
        _ = wait_for_shutdown_trigger(shutdown_rx.clone()) => {
            info!("server node bootstrap setup listener received shutdown request");
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
            let outcome = server_task
                .await
                .context("bootstrap setup server task join failure")?;
            outcome.context("bootstrap setup server exited during shutdown")?;
            return Ok(());
        }
    };
    if completion.is_none() {
        let outcome = server_task
            .await
            .context("bootstrap setup server task join failure")?;
        return outcome.context("bootstrap setup server exited");
    }

    handle.graceful_shutdown(Some(Duration::from_secs(0)));
    let outcome = server_task
        .await
        .context("bootstrap setup server task join failure")?;
    outcome.context("bootstrap setup server exited during transition")?;

    let completion = completion.expect("checked is_some above");
    run_inner(
        completion.config,
        Some(log_buffer),
        runtime_log_control,
        shutdown_rx,
    )
    .await
}

async fn setup_health(State(state): State<SetupServerState>) -> impl IntoResponse {
    let managed = state.managed_state.lock().await;
    (
        StatusCode::OK,
        Json(json!({
            "mode": "bootstrap_setup",
            "state": managed.state,
            "data_dir": state.config.data_dir.display().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
            "revision": git_version::git_version!(fallback = "unknown", args = ["--tags", "--always", "--dirty=-dirty", "--abbrev=12"]),
        })),
    )
}

async fn get_setup_status(State(state): State<SetupServerState>) -> impl IntoResponse {
    let managed = state.managed_state.lock().await.clone();
    let fingerprint = parse_certificate_details_from_path(&state.config.bootstrap_cert_path)
        .ok()
        .map(|parsed| parsed.certificate_fingerprint);
    (
        StatusCode::OK,
        Json(SetupStatusResponse {
            state: managed.state,
            data_dir: state.config.data_dir.display().to_string(),
            bind_addr: state.config.bind_addr.to_string(),
            bootstrap_tls_cert_path: state.config.bootstrap_cert_path.display().to_string(),
            bootstrap_tls_fingerprint: fingerprint,
            cluster_id: managed.cluster_id,
            node_id: managed.node_id,
            recovery_reason: managed.recovery_reason,
            metadata_backend: managed
                .metadata_backend
                .unwrap_or_else(default_setup_metadata_backend),
            available_metadata_backends: available_setup_metadata_backends(),
            pending_join_request: managed.pending_join_request,
        }),
    )
}

async fn start_new_cluster(
    State(state): State<SetupServerState>,
    Json(request): Json<SetupStartClusterRequest>,
) -> impl IntoResponse {
    if let Err(message) = validate_admin_password(&request.admin_password) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    }
    let metadata_backend = request
        .metadata_backend
        .unwrap_or_else(default_setup_metadata_backend);
    let runtime_metadata_backend = match metadata_backend.into_runtime_kind() {
        Ok(backend) => backend,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    {
        let managed = state.managed_state.lock().await;
        if managed.state == SetupLifecycleState::Online {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "node is already initialized" })),
            )
                .into_response();
        }
        if managed.state == SetupLifecycleState::Recovery {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "node requires cluster rejoin; import a fresh node enrollment package instead of starting a new cluster"
                })),
            )
                .into_response();
        }
    }

    let public_origin = match normalize_https_origin(&request.public_origin) {
        Ok(origin) => origin,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let runtime_enrollment_path = runtime_node_enrollment_path(&state.config.data_dir);
    let cluster_id = Uuid::now_v7();
    let node_id = NodeId::new_v4();
    let labels = default_setup_labels();
    let bind_addr = state.config.bind_addr;
    let internal_bind_addr = default_internal_bind_addr(bind_addr);
    let managed_rendezvous_bind_addr = default_managed_rendezvous_bind_addr(bind_addr);
    let public_url = origin_to_string(&public_origin);
    let internal_url = match derive_internal_url(&public_origin, internal_bind_addr.port()) {
        Ok(url) => url,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    let managed_rendezvous_public_url =
        match derive_managed_rendezvous_url(&public_origin, managed_rendezvous_bind_addr.port()) {
            Ok(url) => url,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        };

    let bootstrap = TransportNodeBootstrap {
        version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
        cluster_id,
        node_id,
        mode: NodeBootstrapMode::Cluster,
        data_dir: state.config.data_dir.display().to_string(),
        bind_addr: bind_addr.to_string(),
        public_url: Some(public_url.clone()),
        labels: labels.clone(),
        public_tls: Some(managed_public_tls_files()),
        public_ca_cert_path: Some(MANAGED_PUBLIC_CA_CERT_PATH.to_string()),
        public_peer_api_enabled: false,
        internal_bind_addr: Some(internal_bind_addr.to_string()),
        internal_url: Some(internal_url.clone()),
        internal_tls: Some(managed_internal_tls_files()),
        rendezvous_urls: vec![managed_rendezvous_public_url.clone()],
        rendezvous_mtls_required: true,
        direct_endpoints: vec![
            BootstrapEndpoint {
                url: public_url.clone(),
                usage: Some(BootstrapEndpointUse::PublicApi),
                node_id: Some(node_id),
            },
            BootstrapEndpoint {
                url: internal_url.clone(),
                usage: Some(BootstrapEndpointUse::PeerApi),
                node_id: Some(node_id),
            },
        ],
        relay_mode: RelayMode::Fallback,
        trust_roots: BootstrapTrustRoots {
            cluster_ca_pem: None,
            public_api_ca_pem: None,
            rendezvous_ca_pem: None,
        },
        enrollment_issuer_url: Some(public_url.clone()),
    };

    let artifacts = match issue_self_managed_cluster_artifacts(bootstrap) {
        Ok(artifacts) => artifacts,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    if let Err(err) = artifacts.package.write_to_path(&runtime_enrollment_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    if let Err(err) = write_managed_signer_material(
        &state.config.data_dir,
        &artifacts.ca_cert_pem,
        &artifacts.ca_key_pem,
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    let (managed_rendezvous_cert_pem, managed_rendezvous_key_pem) =
        match issue_managed_rendezvous_tls_identity_from_ca(
            cluster_id,
            &managed_rendezvous_public_url,
            &artifacts.ca_cert_pem,
            &artifacts.ca_key_pem,
        ) {
            Ok(material) => material,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        };
    if let Err(err) = write_managed_rendezvous_material(
        &state.config.data_dir,
        Some(&artifacts.ca_cert_pem),
        &managed_rendezvous_cert_pem,
        &managed_rendezvous_key_pem,
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    if let Err(err) =
        apply_setup_telemetry_choice(&state.config.data_dir, request.telemetry_enabled).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }

    let mut managed = state.managed_state.lock().await;
    managed.state = SetupLifecycleState::Online;
    managed.updated_at_unix = unix_ts();
    managed.cluster_id = Some(cluster_id);
    managed.node_id = Some(node_id);
    managed.runtime_node_enrollment_path = None;
    managed.runtime_data_dir = Some(state.config.data_dir.display().to_string());
    managed.recovery_reason = None;
    managed.metadata_backend = Some(metadata_backend);
    managed.admin_password_hash = Some(hash_admin_password(&request.admin_password));
    managed.managed_rendezvous_bind_addr = Some(managed_rendezvous_bind_addr.to_string());
    managed.managed_rendezvous_public_url = Some(managed_rendezvous_public_url.clone());
    managed.pending_join_request = None;
    if let Err(err) = write_managed_setup_state(&state.config.state_path, &managed) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    let managed_snapshot = managed.clone();
    drop(managed);

    let mut config = match ServerNodeConfig::from_enrollment_path_with_metadata_backend(
        &runtime_enrollment_path,
        runtime_metadata_backend,
    ) {
        Ok(config) => config,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    apply_managed_signer_paths(&state.config.data_dir, &mut config);
    apply_managed_rendezvous_config(&state.config.data_dir, &managed_snapshot, &mut config);
    config.admin_password_hash = Some(hash_admin_password(&request.admin_password));
    let completion_permit = match state.completion_tx.clone().reserve_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed scheduling runtime transition" })),
            )
                .into_response();
        }
    };
    spawn_setup_runtime_transition(completion_permit, config);

    (
        StatusCode::CREATED,
        Json(SetupTransitionResponse {
            status: "transitioning_to_online",
            cluster_id,
            node_id,
            public_url: Some(public_url),
            metadata_backend,
            restart_required: false,
        }),
    )
        .into_response()
}

async fn generate_join_request(
    State(state): State<SetupServerState>,
    Json(request): Json<SetupGenerateJoinRequest>,
) -> impl IntoResponse {
    let public_origin = match normalize_https_origin(&request.public_origin) {
        Ok(origin) => origin,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let mut managed = state.managed_state.lock().await;
    if managed.state == SetupLifecycleState::Online {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "node is already initialized" })),
        )
            .into_response();
    }
    let node_id = managed.node_id.unwrap_or_else(NodeId::new_v4);
    let internal_bind_addr = default_internal_bind_addr(state.config.bind_addr);
    let join_request = NodeJoinRequest {
        version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
        node_id,
        mode: NodeBootstrapMode::Cluster,
        // Transport v1 still requires this compatibility field. Managed import replaces it with
        // the receiving node's local runtime data root, so no issuer-side host path is exported.
        data_dir: ".".to_string(),
        bind_addr: state.config.bind_addr.to_string(),
        public_url: Some(origin_to_string(&public_origin)),
        labels: default_setup_labels(),
        public_tls: Some(managed_public_tls_files()),
        public_ca_cert_path: Some(MANAGED_PUBLIC_CA_CERT_PATH.to_string()),
        public_peer_api_enabled: false,
        internal_bind_addr: Some(internal_bind_addr.to_string()),
        internal_url: Some(
            match derive_internal_url(&public_origin, internal_bind_addr.port()) {
                Ok(url) => url,
                Err(err) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": err.to_string() })),
                    )
                        .into_response();
                }
            },
        ),
        internal_tls: Some(managed_internal_tls_files()),
    };
    managed.state = SetupLifecycleState::PendingJoin;
    managed.updated_at_unix = unix_ts();
    managed.node_id = Some(node_id);
    managed.runtime_data_dir = Some(state.config.data_dir.display().to_string());
    managed.recovery_reason = None;
    managed.pending_join_request = Some(join_request.clone());
    if let Err(err) = write_managed_setup_state(&state.config.state_path, &managed) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(join_request)).into_response()
}

async fn import_node_enrollment_package(
    State(state): State<SetupServerState>,
    Json(request): Json<SetupImportEnrollmentRequest>,
) -> impl IntoResponse {
    if let Err(message) = validate_admin_password(&request.admin_password) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    }

    let package = match NodeEnrollmentPackage::from_json_str(&request.package_json) {
        Ok(package) => package,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    let package = match canonical_managed_node_enrollment(package, &state.config.data_dir) {
        Ok(package) => package,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let mut managed = state.managed_state.lock().await;
    let metadata_backend = match resolve_setup_metadata_backend(request.metadata_backend, &managed)
    {
        Ok(backend) => backend,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    let runtime_metadata_backend = match metadata_backend.into_runtime_kind() {
        Ok(backend) => backend,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    if managed.state == SetupLifecycleState::Recovery {
        if let Some(expected_node_id) = managed.node_id
            && package.bootstrap.node_id != expected_node_id
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "imported node enrollment does not match the recovering node identity"
                })),
            )
                .into_response();
        }
        if let Some(expected_cluster_id) = managed.cluster_id
            && package.bootstrap.cluster_id != expected_cluster_id
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "imported node enrollment does not match the recovering cluster identity"
                })),
            )
                .into_response();
        }
    }
    if let Some(join_request) = managed.pending_join_request.as_ref()
        && package.bootstrap.node_id != join_request.node_id
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "imported node enrollment does not match the pending join request" })),
        )
            .into_response();
    }

    let runtime_enrollment_path = runtime_node_enrollment_path(&state.config.data_dir);
    if let Err(err) = package.write_to_path(&runtime_enrollment_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    if let Err(err) =
        apply_setup_telemetry_choice(&state.config.data_dir, request.telemetry_enabled).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }

    managed.state = SetupLifecycleState::Online;
    managed.updated_at_unix = unix_ts();
    managed.cluster_id = Some(package.bootstrap.cluster_id);
    managed.node_id = Some(package.bootstrap.node_id);
    managed.runtime_node_enrollment_path = None;
    managed.runtime_data_dir = Some(state.config.data_dir.display().to_string());
    managed.recovery_reason = None;
    managed.metadata_backend = Some(metadata_backend);
    managed.admin_password_hash = Some(hash_admin_password(&request.admin_password));
    managed.managed_rendezvous_bind_addr = None;
    managed.managed_rendezvous_public_url = None;
    managed.pending_join_request = None;
    if let Err(err) = write_managed_setup_state(&state.config.state_path, &managed) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }
    let managed_snapshot = managed.clone();
    drop(managed);

    let mut config = match ServerNodeConfig::from_enrollment_path_with_metadata_backend(
        &runtime_enrollment_path,
        runtime_metadata_backend,
    ) {
        Ok(config) => config,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    apply_managed_signer_paths(&state.config.data_dir, &mut config);
    apply_managed_rendezvous_config(&state.config.data_dir, &managed_snapshot, &mut config);
    config.admin_password_hash = Some(hash_admin_password(&request.admin_password));
    let completion_permit = match state.completion_tx.clone().reserve_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed scheduling runtime transition" })),
            )
                .into_response();
        }
    };
    spawn_setup_runtime_transition(completion_permit, config);

    (
        StatusCode::CREATED,
        Json(SetupTransitionResponse {
            status: "transitioning_to_online",
            cluster_id: package.bootstrap.cluster_id,
            node_id: package.bootstrap.node_id,
            public_url: package.bootstrap.public_url.clone(),
            metadata_backend,
            restart_required: false,
        }),
    )
        .into_response()
}

fn resolve_setup_metadata_backend(
    requested: Option<SetupMetadataBackend>,
    managed: &ManagedSetupState,
) -> Result<SetupMetadataBackend> {
    if managed.state != SetupLifecycleState::Recovery {
        return Ok(requested.unwrap_or_else(default_setup_metadata_backend));
    }

    let persisted = managed
        .metadata_backend
        .context("recovering managed setup state is missing metadata_backend")?;
    if requested.is_some_and(|backend| backend != persisted) {
        bail!("metadata backend cannot be changed while recovering an initialized node");
    }
    Ok(persisted)
}

fn explicit_runtime_env_vars_present() -> Vec<&'static str> {
    [
        "IRONMESH_NODE_ENROLLMENT_FILE",
        "IRONMESH_NODE_BOOTSTRAP_FILE",
        "IRONMESH_NODE_ID",
        "IRONMESH_CLUSTER_ID",
        "IRONMESH_PUBLIC_URL",
        "IRONMESH_PUBLIC_TLS_CERT",
        "IRONMESH_PUBLIC_TLS_KEY",
        "IRONMESH_PUBLIC_TLS_CA_CERT",
        "IRONMESH_PUBLIC_TLS_CA_KEY",
        "IRONMESH_INTERNAL_BIND",
        "IRONMESH_INTERNAL_URL",
        "IRONMESH_INTERNAL_TLS_CA_CERT",
        "IRONMESH_INTERNAL_TLS_CERT",
        "IRONMESH_INTERNAL_TLS_KEY",
        "IRONMESH_INTERNAL_TLS_CA_KEY",
        "IRONMESH_RENDEZVOUS_URLS",
        "IRONMESH_RENDEZVOUS_CA_CERT",
        "IRONMESH_RENDEZVOUS_MTLS_REQUIRED",
        "IRONMESH_RELAY_MODE",
        "IRONMESH_ADMIN_TOKEN",
        "IRONMESH_REQUIRE_CLIENT_AUTH",
    ]
    .iter()
    .copied()
    .filter(|key| {
        std::env::var(*key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    })
    .collect()
}

fn runtime_enrollment_recovery_reason(
    config: &ServerNodeConfig,
) -> Option<ManagedRecoveryReasonCode> {
    let status = collect_node_certificate_status(
        config
            .public_tls
            .as_ref()
            .map(|tls| tls.cert_path.as_path()),
        config
            .public_tls
            .as_ref()
            .and_then(|tls| tls.metadata_path.as_deref()),
        config
            .internal_tls
            .as_ref()
            .map(|tls| tls.cert_path.as_path()),
        config
            .internal_tls
            .as_ref()
            .and_then(|tls| tls.metadata_path.as_deref()),
        NodeCertificateAutoRenewStatusView {
            enabled: false,
            enrollment_path: None,
            issuer_url: None,
            check_interval_secs: None,
            last_attempt_unix: None,
            last_success_unix: None,
            last_error: None,
            restart_required: false,
        },
    );

    let recovery_reason = if node_certificate_is_expired(&status.public_tls)
        || node_certificate_is_expired(&status.internal_tls)
    {
        Some(ManagedRecoveryReasonCode::CertificateExpired)
    } else if node_certificate_is_missing(&status.public_tls)
        || node_certificate_is_missing(&status.internal_tls)
    {
        Some(ManagedRecoveryReasonCode::CertificateMaterialMissing)
    } else {
        None
    };
    if recovery_reason.is_some() {
        log_certificate_lifecycle_status(&status);
        tracing::warn!(
            node_id = %config.node_id,
            data_dir = %config.data_dir.display(),
            "managed runtime enrollment is unusable; starting setup recovery mode"
        );
    }
    recovery_reason
}

fn node_certificate_is_expired(status: &NodeCertificateStatusView) -> bool {
    status.state == NodeCertificateLifecycleState::Expired
}

fn node_certificate_is_missing(status: &NodeCertificateStatusView) -> bool {
    status.state == NodeCertificateLifecycleState::Missing
}

fn transition_managed_setup_state_to_recovery(
    state_path: &std::path::Path,
    managed_state: &mut ManagedSetupState,
    reason: ManagedRecoveryReason,
) -> Result<()> {
    if managed_state.state == SetupLifecycleState::Recovery
        && managed_state
            .recovery_reason
            .as_ref()
            .is_some_and(|existing| existing.code == reason.code)
    {
        tracing::info!(
            state_path = %state_path.display(),
            cluster_id = ?managed_state.cluster_id,
            node_id = ?managed_state.node_id,
            recovery_reason = ?reason.code,
            "managed setup state is already in recovery"
        );
        return Ok(());
    }

    let previous_state = managed_state.state.clone();
    managed_state.state = SetupLifecycleState::Recovery;
    managed_state.updated_at_unix = unix_ts();
    managed_state.pending_join_request = None;
    managed_state.recovery_reason = Some(reason);
    tracing::warn!(
        state_path = %state_path.display(),
        previous_state = ?previous_state,
        cluster_id = ?managed_state.cluster_id,
        node_id = ?managed_state.node_id,
        recovery_reason = ?managed_state.recovery_reason,
        "managed setup state transitioned to recovery"
    );
    write_managed_setup_state(state_path, managed_state)
}

fn default_setup_bootstrap_config() -> Result<SetupBootstrapConfig> {
    let data_dir = PathBuf::from(
        std::env::var("BERRYKEEP_SERVER_NODE_DATA_DIR")
            .or_else(|_| std::env::var("IRONMESH_DATA_DIR"))
            .unwrap_or_else(|_| "./data/server-node".to_string()),
    );
    let bind_addr: SocketAddr = std::env::var("BERRYKEEP_SERVER_NODE_BIND")
        .or_else(|_| std::env::var("IRONMESH_SERVER_BIND"))
        .unwrap_or_else(|_| "0.0.0.0:8443".to_string())
        .parse()
        .context("invalid server-node bind address for bootstrap setup mode")?;
    managed_startup_bootstrap_config(data_dir, bind_addr)
}

pub(crate) fn managed_startup_bootstrap_config(
    data_dir: PathBuf,
    bind_addr: SocketAddr,
) -> Result<SetupBootstrapConfig> {
    let data_dir = absolute_local_path(&data_dir, "managed setup data directory")?;
    Ok(SetupBootstrapConfig {
        state_path: managed_setup_state_path(&data_dir),
        bootstrap_cert_path: bootstrap_setup_cert_path(&data_dir),
        bootstrap_key_path: bootstrap_setup_key_path(&data_dir),
        data_dir,
        bind_addr,
    })
}

fn managed_setup_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("managed")
}

pub(crate) fn managed_setup_state_path(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir).join("setup-state.json")
}

fn bootstrap_setup_cert_path(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir)
        .join("bootstrap-ui")
        .join("bootstrap-cert.pem")
}

fn bootstrap_setup_key_path(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir)
        .join("bootstrap-ui")
        .join("bootstrap-key.pem")
}

fn runtime_node_enrollment_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join(runtime_node_enrollment_relative_path())
}

/// Fixed runtime enrollment path relative to the managed setup data directory.
///
/// Setup-state v2 derives this path and does not persist it. The relative form remains available
/// only to locate and migrate setup-state v1 artifacts without joining `data_dir` twice.
fn runtime_node_enrollment_relative_path() -> PathBuf {
    PathBuf::from("managed")
        .join("runtime")
        .join("node-enrollment.json")
}

fn managed_internal_tls_files() -> BootstrapTlsFiles {
    BootstrapTlsFiles {
        ca_cert_path: MANAGED_INTERNAL_CA_CERT_PATH.to_string(),
        cert_path: MANAGED_INTERNAL_CERT_PATH.to_string(),
        key_path: MANAGED_INTERNAL_KEY_PATH.to_string(),
    }
}

fn managed_public_tls_files() -> BootstrapServerTlsFiles {
    BootstrapServerTlsFiles {
        cert_path: MANAGED_PUBLIC_CERT_PATH.to_string(),
        key_path: MANAGED_PUBLIC_KEY_PATH.to_string(),
    }
}

fn absolute_local_path(path: &std::path::Path, label: &str) -> Result<PathBuf> {
    ensure_non_traversing_path(path, label)?;
    let normalized = normalize_non_traversing_path(path);
    if normalized.is_absolute() {
        return Ok(normalized);
    }
    let current_dir = std::env::current_dir().context("failed resolving current directory")?;
    Ok(normalize_non_traversing_path(&current_dir.join(normalized)))
}

/// Converts the portable enrollment envelope into the local managed runtime model.
///
/// Managed installations intentionally do not treat host paths from an imported enrollment
/// package as configuration. The package supplies identity and credential material; the local
/// runtime data root and this fixed role-based layout determine where that material lives.
fn canonical_managed_node_enrollment(
    mut package: NodeEnrollmentPackage,
    runtime_data_dir: &std::path::Path,
) -> Result<NodeEnrollmentPackage> {
    package.validate()?;
    let runtime_data_dir = absolute_local_path(runtime_data_dir, "managed runtime data directory")?;
    package.bootstrap.data_dir = runtime_data_dir.display().to_string();

    if package.bootstrap.public_tls.is_some() {
        package.bootstrap.public_tls = Some(managed_public_tls_files());
    }
    if package.bootstrap.public_ca_cert_path.is_some()
        || package.public_tls_material.is_some()
        || package.bootstrap.trust_roots.public_api_ca_pem.is_some()
    {
        package.bootstrap.public_ca_cert_path = Some(MANAGED_PUBLIC_CA_CERT_PATH.to_string());
    }
    if package.bootstrap.internal_tls.is_some() {
        package.bootstrap.internal_tls = Some(managed_internal_tls_files());
    }

    package.validate()?;
    Ok(package)
}

fn managed_runtime_data_dir(
    state: &ManagedSetupState,
    package: &NodeEnrollmentPackage,
) -> Result<PathBuf> {
    let raw = state
        .runtime_data_dir
        .as_deref()
        .unwrap_or(package.bootstrap.data_dir.as_str());
    absolute_local_path(std::path::Path::new(raw), "managed runtime data directory")
}

fn resolve_legacy_managed_enrollment_path(
    setup_data_dir: &std::path::Path,
    raw_path: &str,
) -> Result<PathBuf> {
    match resolve_materialized_path(setup_data_dir, raw_path) {
        Ok(path) => Ok(path),
        Err(original_error) => {
            let candidate = std::path::Path::new(raw_path);
            if !candidate.is_absolute() || !candidate.exists() {
                return Err(original_error);
            }

            let canonical_setup_data_dir = std::fs::canonicalize(setup_data_dir)
                .with_context(|| format!("failed resolving {}", setup_data_dir.display()))?;
            let canonical_candidate = std::fs::canonicalize(candidate)
                .with_context(|| format!("failed resolving {}", candidate.display()))?;
            if !canonical_candidate.starts_with(&canonical_setup_data_dir) {
                return Err(original_error);
            }
            Ok(canonical_candidate)
        }
    }
}

fn find_managed_enrollment_path(
    config: &SetupBootstrapConfig,
    state: &ManagedSetupState,
) -> Result<PathBuf> {
    let canonical_path = runtime_node_enrollment_path(&config.data_dir);
    if canonical_path.exists() {
        return Ok(canonical_path);
    }
    let raw_path = state
        .runtime_node_enrollment_path
        .as_deref()
        .context("managed runtime enrollment path is missing")?;
    resolve_legacy_managed_enrollment_path(&config.data_dir, raw_path)
}

pub(crate) fn managed_signer_dir(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir).join("signer")
}

pub(crate) fn managed_signer_ca_cert_path(data_dir: &std::path::Path) -> PathBuf {
    managed_signer_dir(data_dir).join("cluster-ca.pem")
}

pub(crate) fn managed_signer_ca_key_path(data_dir: &std::path::Path) -> PathBuf {
    managed_signer_dir(data_dir).join("cluster-ca.key")
}

fn managed_runtime_internal_ca_cert_path(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir)
        .join("runtime")
        .join("internal")
        .join("cluster-ca.pem")
}

fn managed_rendezvous_dir(data_dir: &std::path::Path) -> PathBuf {
    managed_setup_dir(data_dir).join("rendezvous")
}

pub(crate) fn managed_rendezvous_cert_path(data_dir: &std::path::Path) -> PathBuf {
    managed_rendezvous_dir(data_dir).join("rendezvous.pem")
}

pub(crate) fn managed_rendezvous_key_path(data_dir: &std::path::Path) -> PathBuf {
    managed_rendezvous_dir(data_dir).join("rendezvous.key")
}

/// Applies the operator's setup-time reliability-telemetry disclosure choice (doc Section 4.4) by
/// writing through to the exact same persisted override the post-setup admin toggle uses
/// (`ReliabilityTelemetryRuntime::set_enabled_override`, backing `PUT /api/v1/auth/telemetry/settings`).
/// There is deliberately no separate "setup choice" storage: once this call persists, the choice is
/// indistinguishable from an admin having flipped the toggle in `server-admin` right after setup.
async fn apply_setup_telemetry_choice(
    data_dir: &std::path::Path,
    telemetry_enabled: bool,
) -> Result<()> {
    let mut runtime = reliability_telemetry::ReliabilityTelemetryRuntime::load(data_dir);
    runtime.set_enabled_override(Some(telemetry_enabled)).await
}

fn ensure_managed_setup_state(path: &std::path::Path) -> Result<ManagedSetupState> {
    if let Some(mut existing) = read_managed_setup_state(path)? {
        migrate_managed_setup_metadata_backend(path, &mut existing)?;
        return Ok(existing);
    }
    let state = ManagedSetupState::default();
    write_managed_setup_state(path, &state)?;
    Ok(state)
}

pub(crate) fn read_managed_setup_state(
    path: &std::path::Path,
) -> Result<Option<ManagedSetupState>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let state = serde_json::from_str::<ManagedSetupState>(&raw)
        .with_context(|| format!("failed parsing {}", path.display()))?;
    if !matches!(
        state.version,
        LEGACY_SETUP_STATE_VERSION | LEGACY_SETUP_STATE_VERSION_2 | SETUP_STATE_VERSION
    ) {
        bail!("unsupported managed setup state version {}", state.version);
    }
    Ok(Some(state))
}

pub(crate) fn write_managed_setup_state(
    path: &std::path::Path,
    state: &ManagedSetupState,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let mut persisted_state = state.clone();
    populate_legacy_metadata_backend_from_env(&mut persisted_state)?;
    persisted_state.version = SETUP_STATE_VERSION;
    persisted_state.runtime_node_enrollment_path = None;
    let payload = serde_json::to_string_pretty(&persisted_state)
        .context("failed serializing managed setup state")?;
    std::fs::write(path, payload).with_context(|| format!("failed writing {}", path.display()))
}

fn migrate_managed_setup_metadata_backend(
    path: &std::path::Path,
    state: &mut ManagedSetupState,
) -> Result<bool> {
    if state.metadata_backend.is_some() {
        return Ok(false);
    }
    let legacy_env_value = std::env::var(METADATA_BACKEND_ENV).ok();
    migrate_managed_setup_metadata_backend_with_value(path, state, legacy_env_value.as_deref())
}

fn migrate_managed_setup_metadata_backend_with_value(
    path: &std::path::Path,
    state: &mut ManagedSetupState,
    legacy_env_value: Option<&str>,
) -> Result<bool> {
    if state.metadata_backend.is_some() {
        return Ok(false);
    }
    if state.version >= SETUP_STATE_VERSION {
        bail!(
            "managed setup state version {} is missing metadata_backend",
            state.version
        );
    }

    let metadata_backend = legacy_setup_metadata_backend(legacy_env_value)?;
    state.metadata_backend = Some(metadata_backend);
    state.version = SETUP_STATE_VERSION;
    state.updated_at_unix = unix_ts();
    write_managed_setup_state(path, state)?;
    tracing::info!(
        state_path = %path.display(),
        metadata_backend = ?metadata_backend,
        source = if legacy_env_value.is_some() {
            METADATA_BACKEND_ENV
        } else {
            "legacy_sqlite_default"
        },
        "managed setup state persisted legacy metadata backend selection"
    );
    Ok(true)
}

fn populate_legacy_metadata_backend_from_env(state: &mut ManagedSetupState) -> Result<()> {
    if state.metadata_backend.is_some() {
        return Ok(());
    }
    if state.version >= SETUP_STATE_VERSION {
        bail!(
            "managed setup state version {} is missing metadata_backend",
            state.version
        );
    }
    let legacy_env_value = std::env::var(METADATA_BACKEND_ENV).ok();
    state.metadata_backend = Some(legacy_setup_metadata_backend(legacy_env_value.as_deref())?);
    Ok(())
}

fn legacy_setup_metadata_backend(raw: Option<&str>) -> Result<SetupMetadataBackend> {
    match raw {
        Some(raw) => parse_metadata_backend(raw).map(SetupMetadataBackend::from),
        None => Ok(SetupMetadataBackend::Sqlite),
    }
}

pub(crate) fn write_managed_signer_material(
    data_dir: &std::path::Path,
    ca_cert_pem: &str,
    ca_key_pem: &str,
) -> Result<()> {
    let signer_dir = managed_signer_dir(data_dir);
    std::fs::create_dir_all(&signer_dir)
        .with_context(|| format!("failed creating {}", signer_dir.display()))?;
    let cert_path = managed_signer_ca_cert_path(data_dir);
    let key_path = managed_signer_ca_key_path(data_dir);
    std::fs::write(&cert_path, ca_cert_pem)
        .with_context(|| format!("failed writing {}", cert_path.display()))?;
    std::fs::write(&key_path, ca_key_pem)
        .with_context(|| format!("failed writing {}", key_path.display()))?;
    Ok(())
}

pub(crate) fn apply_managed_signer_paths(
    data_dir: &std::path::Path,
    config: &mut ServerNodeConfig,
) {
    let key_path = managed_signer_ca_key_path(data_dir);
    if key_path.exists() {
        config.internal_ca_key_path = Some(key_path.clone());
        config.public_ca_key_path = Some(key_path);
    }
}

pub(crate) fn write_managed_rendezvous_material(
    data_dir: &std::path::Path,
    client_ca_cert_pem: Option<&str>,
    cert_pem: &str,
    key_pem: &str,
) -> Result<()> {
    if let Some(client_ca_cert_pem) = client_ca_cert_pem {
        let client_ca_cert_path = managed_runtime_internal_ca_cert_path(data_dir);
        if let Some(parent) = client_ca_cert_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        std::fs::write(&client_ca_cert_path, client_ca_cert_pem)
            .with_context(|| format!("failed writing {}", client_ca_cert_path.display()))?;
    }

    let rendezvous_dir = managed_rendezvous_dir(data_dir);
    std::fs::create_dir_all(&rendezvous_dir)
        .with_context(|| format!("failed creating {}", rendezvous_dir.display()))?;
    let cert_path = managed_rendezvous_cert_path(data_dir);
    let key_path = managed_rendezvous_key_path(data_dir);
    std::fs::write(&cert_path, cert_pem)
        .with_context(|| format!("failed writing {}", cert_path.display()))?;
    std::fs::write(&key_path, key_pem)
        .with_context(|| format!("failed writing {}", key_path.display()))?;
    Ok(())
}

fn apply_managed_rendezvous_config(
    data_dir: &std::path::Path,
    managed_state: &ManagedSetupState,
    config: &mut ServerNodeConfig,
) {
    let Some(bind_addr) = managed_state
        .managed_rendezvous_bind_addr
        .as_deref()
        .and_then(|raw| raw.parse::<SocketAddr>().ok())
    else {
        return;
    };
    let Some(public_url) = managed_state.managed_rendezvous_public_url.clone() else {
        return;
    };

    let client_ca_cert_path = managed_runtime_internal_ca_cert_path(data_dir);
    let cert_path = managed_rendezvous_cert_path(data_dir);
    let key_path = managed_rendezvous_key_path(data_dir);
    if !client_ca_cert_path.exists() || !cert_path.exists() || !key_path.exists() {
        return;
    }

    let canonical_public_url = reqwest::Url::parse(&public_url)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| public_url.clone());
    if config
        .rendezvous_urls
        .iter()
        .map(|existing| {
            reqwest::Url::parse(existing)
                .map(|url| url.to_string())
                .unwrap_or_else(|_| existing.clone())
        })
        .all(|existing| existing != canonical_public_url)
    {
        config.rendezvous_urls.push(canonical_public_url.clone());
    }
    config.rendezvous_registration_enabled = true;
    config.rendezvous_mtls_required = true;
    if config.rendezvous_ca_cert_path.is_none() {
        config.rendezvous_ca_cert_path = Some(client_ca_cert_path.clone());
    }
    config.managed_rendezvous = Some(ManagedRendezvousConfig {
        bind_addr,
        public_url,
        client_ca_cert_path,
        cert_path,
        key_path,
    });
}

fn validate_backup_passphrase(passphrase: &str) -> std::result::Result<(), &'static str> {
    if passphrase.trim().len() < 12 {
        return Err("backup passphrase must be at least 12 characters long");
    }
    Ok(())
}

fn derive_managed_signer_backup_key(
    passphrase: &str,
    salt: &[u8],
    rounds: u32,
) -> [u8; MANAGED_SIGNER_BACKUP_KEY_LEN] {
    let mut key = [0u8; MANAGED_SIGNER_BACKUP_KEY_LEN];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, rounds, &mut key);
    key
}

pub(crate) fn export_managed_signer_backup(
    cluster_id: ClusterId,
    source_node_id: NodeId,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    passphrase: &str,
) -> Result<ManagedSignerBackup> {
    validate_backup_passphrase(passphrase).map_err(anyhow::Error::msg)?;
    let exported_at_unix = unix_ts();
    let plaintext = ManagedSignerBackupPlaintext {
        cluster_id,
        source_node_id,
        exported_at_unix,
        ca_cert_pem: ca_cert_pem.to_string(),
        ca_key_pem: ca_key_pem.to_string(),
    };
    let plaintext_json =
        serde_json::to_vec(&plaintext).context("failed serializing managed signer backup")?;

    let mut salt = [0u8; MANAGED_SIGNER_BACKUP_SALT_LEN];
    let mut nonce = [0u8; MANAGED_SIGNER_BACKUP_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let key =
        derive_managed_signer_backup_key(passphrase, &salt, MANAGED_SIGNER_BACKUP_PBKDF2_ROUNDS);
    let cipher = Aes256GcmSiv::new_from_slice(&key)
        .context("failed initializing managed signer backup cipher")?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext_json.as_ref())
        .map_err(|_| anyhow!("failed encrypting managed signer backup"))?;

    Ok(ManagedSignerBackup {
        version: MANAGED_SIGNER_BACKUP_VERSION,
        cluster_id,
        source_node_id,
        exported_at_unix,
        pbkdf2_rounds: MANAGED_SIGNER_BACKUP_PBKDF2_ROUNDS,
        salt_b64: BASE64_STANDARD.encode(salt),
        nonce_b64: BASE64_STANDARD.encode(nonce),
        ciphertext_b64: BASE64_STANDARD.encode(ciphertext),
    })
}

pub(crate) fn import_managed_signer_backup(
    data_dir: &std::path::Path,
    backup: &ManagedSignerBackup,
    passphrase: &str,
    expected_cluster_id: Option<ClusterId>,
) -> Result<()> {
    if backup.version != MANAGED_SIGNER_BACKUP_VERSION {
        bail!(
            "unsupported managed signer backup version {}",
            backup.version
        );
    }

    let salt = BASE64_STANDARD
        .decode(backup.salt_b64.as_bytes())
        .context("failed decoding managed signer backup salt")?;
    if salt.len() != MANAGED_SIGNER_BACKUP_SALT_LEN {
        bail!("invalid managed signer backup salt length");
    }
    let nonce = BASE64_STANDARD
        .decode(backup.nonce_b64.as_bytes())
        .context("failed decoding managed signer backup nonce")?;
    if nonce.len() != MANAGED_SIGNER_BACKUP_NONCE_LEN {
        bail!("invalid managed signer backup nonce length");
    }
    let ciphertext = BASE64_STANDARD
        .decode(backup.ciphertext_b64.as_bytes())
        .context("failed decoding managed signer backup ciphertext")?;
    let key = derive_managed_signer_backup_key(passphrase, &salt, backup.pbkdf2_rounds);
    let cipher = Aes256GcmSiv::new_from_slice(&key)
        .context("failed initializing managed signer backup cipher")?;
    let plaintext_json = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("failed decrypting managed signer backup"))?;
    let plaintext = serde_json::from_slice::<ManagedSignerBackupPlaintext>(&plaintext_json)
        .context("failed parsing managed signer backup payload")?;

    if plaintext.cluster_id != backup.cluster_id {
        bail!("managed signer backup cluster ID mismatch");
    }
    if plaintext.source_node_id != backup.source_node_id {
        bail!("managed signer backup source node ID mismatch");
    }
    if let Some(expected_cluster_id) = expected_cluster_id
        && plaintext.cluster_id != expected_cluster_id
    {
        bail!(
            "managed signer backup belongs to cluster {} but this node is in cluster {}",
            plaintext.cluster_id,
            expected_cluster_id
        );
    }

    write_managed_signer_material(data_dir, &plaintext.ca_cert_pem, &plaintext.ca_key_pem)
}

pub(crate) fn export_managed_rendezvous_failover_package(
    params: ManagedRendezvousFailoverExportParams<'_>,
) -> Result<ManagedRendezvousFailoverPackage> {
    validate_backup_passphrase(params.passphrase).map_err(anyhow::Error::msg)?;
    let exported_at_unix = unix_ts();
    let plaintext = ManagedRendezvousFailoverPlaintext {
        cluster_id: params.cluster_id,
        source_node_id: params.source_node_id,
        target_node_id: params.target_node_id,
        exported_at_unix,
        public_url: params.public_url.to_string(),
        client_ca_cert_pem: Some(params.client_ca_cert_pem.to_string()),
        cert_pem: params.cert_pem.to_string(),
        key_pem: params.key_pem.to_string(),
    };
    let plaintext_json = serde_json::to_vec(&plaintext)
        .context("failed serializing managed rendezvous failover package")?;

    let mut salt = [0u8; MANAGED_SIGNER_BACKUP_SALT_LEN];
    let mut nonce = [0u8; MANAGED_SIGNER_BACKUP_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let key = derive_managed_signer_backup_key(
        params.passphrase,
        &salt,
        MANAGED_SIGNER_BACKUP_PBKDF2_ROUNDS,
    );
    let cipher = Aes256GcmSiv::new_from_slice(&key)
        .context("failed initializing managed rendezvous failover cipher")?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext_json.as_ref())
        .map_err(|_| anyhow!("failed encrypting managed rendezvous failover package"))?;

    Ok(ManagedRendezvousFailoverPackage {
        version: MANAGED_RENDEZVOUS_FAILOVER_VERSION,
        cluster_id: params.cluster_id,
        source_node_id: params.source_node_id,
        target_node_id: params.target_node_id,
        exported_at_unix,
        public_url: params.public_url.to_string(),
        deployment_target: params.deployment_target,
        includes_cluster_ca_cert: true,
        pbkdf2_rounds: MANAGED_SIGNER_BACKUP_PBKDF2_ROUNDS,
        salt_b64: BASE64_STANDARD.encode(salt),
        nonce_b64: BASE64_STANDARD.encode(nonce),
        ciphertext_b64: BASE64_STANDARD.encode(ciphertext),
    })
}

pub(crate) fn import_managed_rendezvous_failover_package(
    data_dir: &std::path::Path,
    package: &ManagedRendezvousFailoverPackage,
    passphrase: &str,
    bind_addr: SocketAddr,
    expected_cluster_id: Option<ClusterId>,
    expected_node_id: Option<NodeId>,
) -> Result<()> {
    if package.version != MANAGED_RENDEZVOUS_FAILOVER_VERSION {
        bail!(
            "unsupported managed rendezvous failover package version {}",
            package.version
        );
    }

    let salt = BASE64_STANDARD
        .decode(package.salt_b64.as_bytes())
        .context("failed decoding managed rendezvous failover salt")?;
    if salt.len() != MANAGED_SIGNER_BACKUP_SALT_LEN {
        bail!("invalid managed rendezvous failover salt length");
    }
    let nonce = BASE64_STANDARD
        .decode(package.nonce_b64.as_bytes())
        .context("failed decoding managed rendezvous failover nonce")?;
    if nonce.len() != MANAGED_SIGNER_BACKUP_NONCE_LEN {
        bail!("invalid managed rendezvous failover nonce length");
    }
    let ciphertext = BASE64_STANDARD
        .decode(package.ciphertext_b64.as_bytes())
        .context("failed decoding managed rendezvous failover ciphertext")?;
    let key = derive_managed_signer_backup_key(passphrase, &salt, package.pbkdf2_rounds);
    let cipher = Aes256GcmSiv::new_from_slice(&key)
        .context("failed initializing managed rendezvous failover cipher")?;
    let plaintext_json = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("failed decrypting managed rendezvous failover package"))?;
    let plaintext = serde_json::from_slice::<ManagedRendezvousFailoverPlaintext>(&plaintext_json)
        .context("failed parsing managed rendezvous failover payload")?;

    if plaintext.cluster_id != package.cluster_id {
        bail!("managed rendezvous failover cluster ID mismatch");
    }
    if plaintext.source_node_id != package.source_node_id {
        bail!("managed rendezvous failover source node ID mismatch");
    }
    if plaintext.target_node_id != package.target_node_id {
        bail!("managed rendezvous failover target node ID mismatch");
    }
    if plaintext.public_url != package.public_url {
        bail!("managed rendezvous failover public URL mismatch");
    }
    if let Some(expected_cluster_id) = expected_cluster_id
        && plaintext.cluster_id != expected_cluster_id
    {
        bail!(
            "managed rendezvous failover belongs to cluster {} but this node is in cluster {}",
            plaintext.cluster_id,
            expected_cluster_id
        );
    }
    if let Some(expected_node_id) = expected_node_id {
        match plaintext.target_node_id {
            Some(target_node_id) if target_node_id == expected_node_id => {}
            Some(target_node_id) => {
                bail!(
                    "managed rendezvous failover targets node {} but this node is {}",
                    target_node_id,
                    expected_node_id
                );
            }
            None => {
                bail!("managed rendezvous failover package does not target an embedded node");
            }
        }
    }

    write_managed_rendezvous_material(
        data_dir,
        plaintext.client_ca_cert_pem.as_deref(),
        &plaintext.cert_pem,
        &plaintext.key_pem,
    )?;

    let state_path = managed_setup_state_path(data_dir);
    let mut managed_state = ensure_managed_setup_state(&state_path)?;
    managed_state.updated_at_unix = unix_ts();
    managed_state.cluster_id.get_or_insert(plaintext.cluster_id);
    if let Some(target_node_id) = plaintext.target_node_id {
        managed_state.node_id.get_or_insert(target_node_id);
    }
    managed_state.managed_rendezvous_bind_addr = Some(bind_addr.to_string());
    managed_state.managed_rendezvous_public_url = Some(plaintext.public_url);
    write_managed_setup_state(&state_path, &managed_state)
}

async fn ensure_bootstrap_tls_config(config: &SetupBootstrapConfig) -> Result<RustlsConfig> {
    if !config.bootstrap_cert_path.exists() || !config.bootstrap_key_path.exists() {
        let (cert_pem, key_pem) = generate_bootstrap_tls_identity(config.bind_addr)?;
        if let Some(parent) = config.bootstrap_cert_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        std::fs::write(&config.bootstrap_cert_path, cert_pem).with_context(|| {
            format!(
                "failed writing bootstrap certificate {}",
                config.bootstrap_cert_path.display()
            )
        })?;
        std::fs::write(&config.bootstrap_key_path, key_pem).with_context(|| {
            format!(
                "failed writing bootstrap key {}",
                config.bootstrap_key_path.display()
            )
        })?;
    }

    RustlsConfig::from_pem_file(&config.bootstrap_cert_path, &config.bootstrap_key_path)
        .await
        .with_context(|| {
            format!(
                "failed building bootstrap TLS config from {} and {}",
                config.bootstrap_cert_path.display(),
                config.bootstrap_key_path.display()
            )
        })
}

fn generate_bootstrap_tls_identity(bind_addr: SocketAddr) -> Result<(String, String)> {
    let mut params = CertificateParams::new(Vec::new())
        .context("failed creating bootstrap TLS certificate params")?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "ironmesh-bootstrap-ui");
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = OffsetDateTime::from_unix_timestamp(unix_ts().saturating_sub(300) as i64)
        .context("failed setting bootstrap TLS not_before")?;
    params.not_after =
        OffsetDateTime::from_unix_timestamp(unix_ts().saturating_add(30 * 24 * 60 * 60) as i64)
            .context("failed setting bootstrap TLS not_after")?;
    params.subject_alt_names.push(SanType::DnsName(
        "localhost"
            .try_into()
            .context("invalid localhost bootstrap SAN")?,
    ));
    params
        .subject_alt_names
        .push(SanType::IpAddress(Ipv4Addr::LOCALHOST.into()));
    if !bind_addr.ip().is_unspecified() {
        params
            .subject_alt_names
            .push(SanType::IpAddress(bind_addr.ip()));
    }
    if let Some(hostname) = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        params.subject_alt_names.push(SanType::DnsName(
            hostname
                .try_into()
                .context("invalid hostname bootstrap SAN")?,
        ));
    }
    let key_pair = KeyPair::generate().context("failed generating bootstrap TLS keypair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("failed self-signing bootstrap TLS certificate")?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn issue_self_managed_cluster_artifacts(
    mut bootstrap: TransportNodeBootstrap,
) -> Result<SelfManagedClusterArtifacts> {
    bootstrap.validate()?;
    let policy = build_tls_issue_policy(None, None)
        .map_err(|status| anyhow!("invalid TLS policy: {status}"))?;
    let (ca_cert_pem, ca_key_pem) = generate_cluster_ca(bootstrap.cluster_id)?;
    bootstrap.trust_roots = BootstrapTrustRoots {
        cluster_ca_pem: Some(ca_cert_pem.clone()),
        public_api_ca_pem: Some(ca_cert_pem.clone()),
        rendezvous_ca_pem: bootstrap
            .rendezvous_mtls_required
            .then(|| ca_cert_pem.clone()),
    };

    let internal_tls_material =
        issue_internal_node_tls_material_from_ca(&bootstrap, &ca_cert_pem, &ca_key_pem, policy)?;
    let public_tls_material =
        issue_public_node_tls_material_from_ca(&bootstrap, &ca_cert_pem, &ca_key_pem, policy)?;

    let package = NodeEnrollmentPackage {
        bootstrap,
        public_tls_material,
        internal_tls_material: Some(internal_tls_material),
    };
    package.validate()?;
    Ok(SelfManagedClusterArtifacts {
        package,
        ca_cert_pem,
        ca_key_pem,
    })
}

fn generate_cluster_ca(cluster_id: ClusterId) -> Result<(String, String)> {
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating cluster CA params")?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("ironmesh-cluster-{cluster_id}"));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.not_before = OffsetDateTime::from_unix_timestamp(unix_ts().saturating_sub(300) as i64)
        .context("failed setting cluster CA not_before")?;
    params.not_after =
        OffsetDateTime::from_unix_timestamp(unix_ts().saturating_add(3650 * 24 * 60 * 60) as i64)
            .context("failed setting cluster CA not_after")?;
    let key_pair = KeyPair::generate().context("failed generating cluster CA keypair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("failed self-signing cluster CA")?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn issue_internal_node_tls_material_from_ca(
    bootstrap: &TransportNodeBootstrap,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    policy: NodeTlsIssuePolicy,
) -> Result<BootstrapMutualTlsMaterial> {
    let issuer_key = KeyPair::from_pem(ca_key_pem).context("failed parsing cluster CA keypair")?;
    let issuer =
        Issuer::from_ca_cert_pem(ca_cert_pem, issuer_key).context("failed building CA issuer")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating internal TLS params")?;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("ironmesh-node-{}", bootstrap.node_id),
    );
    params.is_ca = IsCa::NoCa;
    params.not_before = OffsetDateTime::from_unix_timestamp(policy.not_before_unix as i64)
        .context("failed setting internal TLS not_before")?;
    params.not_after = OffsetDateTime::from_unix_timestamp(policy.not_after_unix as i64)
        .context("failed setting internal TLS not_after")?;
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::ServerAuth,
    ];
    params.subject_alt_names = build_internal_node_subject_alt_names(bootstrap)?;
    let key_pair = KeyPair::generate().context("failed generating internal TLS keypair")?;
    let cert = params
        .signed_by(&key_pair, &issuer)
        .context("failed signing internal TLS certificate")?;
    let cert_pem = cert.pem();
    let metadata = build_tls_material_metadata(&cert_pem, policy)
        .map_err(|status| anyhow!("failed building internal TLS metadata: {status}"))?;
    Ok(BootstrapMutualTlsMaterial {
        ca_cert_pem: ca_cert_pem.to_string(),
        cert_pem,
        key_pem: key_pair.serialize_pem(),
        metadata,
    })
}

fn issue_public_node_tls_material_from_ca(
    bootstrap: &TransportNodeBootstrap,
    ca_cert_pem: &str,
    ca_key_pem: &str,
    policy: NodeTlsIssuePolicy,
) -> Result<Option<BootstrapMutualTlsMaterial>> {
    if bootstrap.public_tls.is_none() {
        return Ok(None);
    }
    let issuer_key = KeyPair::from_pem(ca_key_pem).context("failed parsing public CA keypair")?;
    let issuer =
        Issuer::from_ca_cert_pem(ca_cert_pem, issuer_key).context("failed building CA issuer")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating public TLS params")?;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("ironmesh-public-{}", bootstrap.node_id),
    );
    params.is_ca = IsCa::NoCa;
    params.not_before = OffsetDateTime::from_unix_timestamp(policy.not_before_unix as i64)
        .context("failed setting public TLS not_before")?;
    params.not_after = OffsetDateTime::from_unix_timestamp(policy.not_after_unix as i64)
        .context("failed setting public TLS not_after")?;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // URLs are mutable reachability locators.  Direct clients bind the
    // connection to these immutable URI SANs, while ordinary HTTPS clients
    // continue to use the DNS/IP SANs supplied by the bootstrap origin.
    let mut subject_alt_names = build_public_node_subject_alt_names(bootstrap)?;
    subject_alt_names.push(SanType::URI(
        format!("urn:ironmesh:node:{}", bootstrap.node_id)
            .try_into()
            .context("invalid IronMesh node identity URI SAN")?,
    ));
    subject_alt_names.push(SanType::URI(
        format!("urn:ironmesh:cluster:{}", bootstrap.cluster_id)
            .try_into()
            .context("invalid IronMesh cluster identity URI SAN")?,
    ));
    params.subject_alt_names = subject_alt_names;
    let key_pair = KeyPair::generate().context("failed generating public TLS keypair")?;
    let cert = params
        .signed_by(&key_pair, &issuer)
        .context("failed signing public TLS certificate")?;
    let cert_pem = cert.pem();
    let metadata = build_tls_material_metadata(&cert_pem, policy)
        .map_err(|status| anyhow!("failed building public TLS metadata: {status}"))?;
    Ok(Some(BootstrapMutualTlsMaterial {
        ca_cert_pem: ca_cert_pem.to_string(),
        cert_pem,
        key_pem: key_pair.serialize_pem(),
        metadata,
    }))
}

fn default_setup_labels() -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("region".to_string(), "local".to_string());
    labels.insert("dc".to_string(), "bootstrap".to_string());
    labels.insert("rack".to_string(), "bootstrap".to_string());
    labels
}

fn default_internal_bind_addr(public_bind_addr: SocketAddr) -> SocketAddr {
    let public_port = public_bind_addr.port();
    let internal_port = if public_port <= u16::MAX - 10_000 {
        public_port + 10_000
    } else if public_port <= u16::MAX - 1_000 {
        public_port + 1_000
    } else {
        18_443
    };
    SocketAddr::new(public_bind_addr.ip(), internal_port)
}

pub(crate) fn default_managed_rendezvous_bind_addr(public_bind_addr: SocketAddr) -> SocketAddr {
    let public_port = public_bind_addr.port();
    let rendezvous_port = if public_port <= u16::MAX - 1_000 {
        public_port + 1_000
    } else if public_port < u16::MAX {
        public_port + 1
    } else {
        9_443
    };
    SocketAddr::new(public_bind_addr.ip(), rendezvous_port)
}

pub(crate) fn hash_admin_password(password: &str) -> String {
    const ROUNDS: u32 = 600_000;
    const SALT_LEN: usize = 16;
    const HASH_LEN: usize = 32;
    let mut salt = [0u8; SALT_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    let mut hash = [0u8; HASH_LEN];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, ROUNDS, &mut hash);
    let salt_hex: String = salt.iter().map(|b| format!("{b:02x}")).collect();
    let hash_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("pbkdf2sha256:{ROUNDS}:{salt_hex}:{hash_hex}")
}

pub(crate) fn validate_admin_password(password: &str) -> std::result::Result<(), &'static str> {
    if password.trim().len() < 12 {
        return Err("admin password must be at least 12 characters long");
    }
    Ok(())
}

fn normalize_https_origin(raw: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(raw.trim())
        .with_context(|| format!("invalid public origin {raw:?}"))?;
    if url.scheme() != "https" {
        bail!("public origin must use https");
    }
    url.set_query(None);
    url.set_fragment(None);
    url.set_path("");
    if url.host_str().is_none() {
        bail!("public origin must include a host");
    }
    Ok(url)
}

fn derive_internal_url(public_origin: &reqwest::Url, port: u16) -> Result<String> {
    let mut url = public_origin.clone();
    url.set_port(Some(port))
        .map_err(|_| anyhow!("failed deriving internal URL port"))?;
    Ok(origin_to_string(&url))
}

fn derive_managed_rendezvous_url(public_origin: &reqwest::Url, port: u16) -> Result<String> {
    let mut url = public_origin.clone();
    url.set_port(Some(port))
        .map_err(|_| anyhow!("failed deriving managed rendezvous URL port"))?;
    Ok(origin_to_string(&url))
}

pub(crate) fn issue_managed_rendezvous_tls_identity_from_ca(
    cluster_id: ClusterId,
    public_url: &str,
    ca_cert_pem: &str,
    ca_key_pem: &str,
) -> Result<(String, String)> {
    let url = reqwest::Url::parse(public_url)
        .with_context(|| format!("invalid rendezvous URL {public_url:?}"))?;
    let host = url
        .host_str()
        .context("managed rendezvous URL must include a host")?;

    let issuer_key = KeyPair::from_pem(ca_key_pem).context("failed parsing cluster CA keypair")?;
    let issuer =
        Issuer::from_ca_cert_pem(ca_cert_pem, issuer_key).context("failed building CA issuer")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating rendezvous TLS params")?;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(
        DnType::CommonName,
        format!("ironmesh-rendezvous-{cluster_id}"),
    );
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = OffsetDateTime::from_unix_timestamp(unix_ts().saturating_sub(300) as i64)
        .context("failed setting managed rendezvous TLS not_before")?;
    params.not_after =
        OffsetDateTime::from_unix_timestamp(unix_ts().saturating_add(3650 * 24 * 60 * 60) as i64)
            .context("failed setting managed rendezvous TLS not_after")?;
    if let Ok(ip_addr) = host.parse::<std::net::IpAddr>() {
        params.subject_alt_names.push(SanType::IpAddress(ip_addr));
    } else {
        params.subject_alt_names.push(SanType::DnsName(
            host.try_into()
                .context("invalid managed rendezvous DNS SAN")?,
        ));
    }
    if host.eq_ignore_ascii_case("localhost") {
        params
            .subject_alt_names
            .push(SanType::IpAddress(Ipv4Addr::LOCALHOST.into()));
    }
    let key_pair = KeyPair::generate().context("failed generating managed rendezvous keypair")?;
    let cert = params
        .signed_by(&key_pair, &issuer)
        .context("failed signing managed rendezvous certificate")?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

fn origin_to_string(url: &reqwest::Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = Uuid::now_v7();
        let path = std::env::temp_dir().join(format!("ironmesh-setup-{name}-{unique}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_cluster_config_without_internal_tls(
        data_dir: impl Into<PathBuf>,
        bind_addr: SocketAddr,
    ) -> ServerNodeConfig {
        let mut labels = HashMap::new();
        labels.insert("region".to_string(), "local".to_string());
        labels.insert("dc".to_string(), "local-dc".to_string());
        labels.insert("rack".to_string(), "local-rack".to_string());

        ServerNodeConfig {
            mode: ServerNodeMode::Cluster,
            cluster_id: Uuid::now_v7(),
            node_id: NodeId::new_v4(),
            data_dir: data_dir.into(),
            metadata_backend: storage::MetadataBackendKind::Sqlite,
            bind_addr,
            public_url: Some(format!("http://{bind_addr}")),
            s3_bind_addr: None,
            s3_public_url: None,
            labels,
            public_tls: None,
            allow_insecure_public_http: true,
            public_ca_cert_path: None,
            public_ca_key_path: None,
            bootstrap_trust_roots: None,
            advertised_direct_endpoints: Vec::new(),
            public_peer_api_enabled: false,
            internal_tls: None,
            internal_ca_key_path: None,
            local_status_bind_addr: None,
            rendezvous_ca_cert_path: None,
            rendezvous_urls: vec![format!("http://{bind_addr}")],
            rendezvous_registration_enabled: false,
            global_rendezvous_registration_enabled: false,
            rendezvous_mtls_required: false,
            managed_rendezvous: None,
            relay_mode: RelayMode::Fallback,
            enrollment_issuer_url: None,
            node_enrollment_path: None,
            node_enrollment_auto_renew_enabled: false,
            node_enrollment_auto_renew_check_secs: node_enrollment_auto_renew_check_secs(),
            heartbeat_timeout_secs: 90,
            audit_interval_secs: 3600,
            replica_view_sync_interval_secs: DEFAULT_REPLICA_VIEW_SYNC_INTERVAL_SECS,
            replication_factor: 1,
            accepted_over_replication_items: 0,
            metadata_commit_mode: MetadataCommitMode::Local,
            autonomous_replication_on_put_enabled: false,
            replication_repair_enabled: false,
            replication_repair_batch_size: 256,
            replication_repair_max_retries: 3,
            replication_repair_backoff_secs: 30,
            repair_busy_throttle_enabled: false,
            repair_busy_inflight_threshold: 32,
            repair_busy_wait_millis: 100,
            startup_repair_enabled: false,
            startup_repair_delay_secs: 5,
            peer_heartbeat_enabled: false,
            peer_heartbeat_interval_secs: 15,
            admin_token: None,
            admin_password_hash: None,
            require_client_auth: false,
        }
    }

    fn test_node_enrollment_package(
        data_dir: &std::path::Path,
        bind_addr: SocketAddr,
    ) -> NodeEnrollmentPackage {
        let node_id = NodeId::new_v4();
        let cluster_id = Uuid::now_v7();
        let internal_bind_addr = default_internal_bind_addr(bind_addr);
        let rendezvous_bind_addr = default_managed_rendezvous_bind_addr(bind_addr);
        let public_url = format!("https://127.0.0.1:{}", bind_addr.port());
        let internal_url = format!("https://127.0.0.1:{}", internal_bind_addr.port());
        let rendezvous_url = format!("https://127.0.0.1:{}", rendezvous_bind_addr.port());

        issue_self_managed_cluster_artifacts(TransportNodeBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            node_id,
            mode: NodeBootstrapMode::Cluster,
            data_dir: data_dir.display().to_string(),
            bind_addr: bind_addr.to_string(),
            public_url: Some(public_url.clone()),
            labels: default_setup_labels(),
            public_tls: Some(BootstrapServerTlsFiles {
                cert_path: "managed/runtime/public/public.pem".to_string(),
                key_path: "managed/runtime/public/public.key".to_string(),
            }),
            public_ca_cert_path: Some("managed/runtime/public/public-ca.pem".to_string()),
            public_peer_api_enabled: false,
            internal_bind_addr: Some(internal_bind_addr.to_string()),
            internal_url: Some(internal_url.clone()),
            internal_tls: Some(BootstrapTlsFiles {
                ca_cert_path: "managed/runtime/internal/cluster-ca.pem".to_string(),
                cert_path: "managed/runtime/internal/node.pem".to_string(),
                key_path: "managed/runtime/internal/node.key".to_string(),
            }),
            rendezvous_urls: vec![rendezvous_url.clone()],
            rendezvous_mtls_required: true,
            direct_endpoints: vec![
                BootstrapEndpoint {
                    url: public_url.clone(),
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(node_id),
                },
                BootstrapEndpoint {
                    url: internal_url,
                    usage: Some(BootstrapEndpointUse::PeerApi),
                    node_id: Some(node_id),
                },
            ],
            relay_mode: RelayMode::Fallback,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            enrollment_issuer_url: Some(public_url),
        })
        .unwrap()
        .package
    }

    #[test]
    fn managed_setup_state_roundtrip() {
        let dir = temp_dir("state-roundtrip");
        let path = managed_setup_state_path(&dir);
        let state = ManagedSetupState {
            version: SETUP_STATE_VERSION,
            state: SetupLifecycleState::PendingJoin,
            updated_at_unix: 123,
            cluster_id: Some(Uuid::now_v7()),
            node_id: Some(NodeId::new_v4()),
            runtime_node_enrollment_path: Some("managed/runtime/node-enrollment.json".to_string()),
            runtime_data_dir: Some(dir.display().to_string()),
            recovery_reason: None,
            metadata_backend: Some(SetupMetadataBackend::Sqlite),
            admin_password_hash: Some(hash_token("super-secret-password")),
            managed_rendezvous_bind_addr: Some("0.0.0.0:9443".to_string()),
            managed_rendezvous_public_url: Some("https://node-a.local:9443".to_string()),
            pending_join_request: Some(NodeJoinRequest {
                version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
                node_id: NodeId::new_v4(),
                mode: NodeBootstrapMode::Cluster,
                data_dir: dir.display().to_string(),
                bind_addr: "0.0.0.0:8443".to_string(),
                public_url: Some("https://node-a.local:8443".to_string()),
                labels: default_setup_labels(),
                public_tls: Some(BootstrapServerTlsFiles {
                    cert_path: "managed/runtime/public/public.pem".to_string(),
                    key_path: "managed/runtime/public/public.key".to_string(),
                }),
                public_ca_cert_path: Some("managed/runtime/public/public-ca.pem".to_string()),
                public_peer_api_enabled: false,
                internal_bind_addr: Some("0.0.0.0:18443".to_string()),
                internal_url: Some("https://node-a.local:18443".to_string()),
                internal_tls: Some(BootstrapTlsFiles {
                    ca_cert_path: "managed/runtime/internal/cluster-ca.pem".to_string(),
                    cert_path: "managed/runtime/internal/node.pem".to_string(),
                    key_path: "managed/runtime/internal/node.key".to_string(),
                }),
            }),
        };
        write_managed_setup_state(&path, &state).unwrap();
        let restored = read_managed_setup_state(&path).unwrap().unwrap();
        assert_eq!(restored.version, SETUP_STATE_VERSION);
        assert_eq!(restored.state, SetupLifecycleState::PendingJoin);
        assert!(restored.runtime_node_enrollment_path.is_none());
        assert_eq!(
            restored.runtime_data_dir.as_deref(),
            Some(dir.to_string_lossy().as_ref())
        );
        assert_eq!(
            restored.admin_password_hash.as_deref(),
            Some(hash_token("super-secret-password").as_str())
        );
        assert_eq!(
            restored
                .pending_join_request
                .as_ref()
                .and_then(|request| request.public_url.as_deref()),
            Some("https://node-a.local:8443")
        );
        assert_eq!(
            restored.managed_rendezvous_public_url.as_deref(),
            Some("https://node-a.local:9443")
        );
        assert_eq!(
            restored.metadata_backend,
            Some(SetupMetadataBackend::Sqlite)
        );
    }

    #[test]
    fn new_managed_setup_state_uses_available_default_backend() {
        let state = ManagedSetupState::default();
        #[cfg(feature = "turso-metadata")]
        assert_eq!(state.metadata_backend, Some(SetupMetadataBackend::Turso));
        #[cfg(not(feature = "turso-metadata"))]
        assert_eq!(state.metadata_backend, Some(SetupMetadataBackend::Sqlite));
    }

    #[test]
    fn legacy_metadata_backend_migration_defaults_to_sqlite_once() {
        let dir = temp_dir("legacy-metadata-backend-default");
        let path = managed_setup_state_path(&dir);
        let mut state = ManagedSetupState {
            version: LEGACY_SETUP_STATE_VERSION_2,
            metadata_backend: None,
            ..ManagedSetupState::default()
        };

        assert!(
            migrate_managed_setup_metadata_backend_with_value(&path, &mut state, None).unwrap()
        );
        assert_eq!(state.metadata_backend, Some(SetupMetadataBackend::Sqlite));
        assert_eq!(state.version, SETUP_STATE_VERSION);

        assert!(
            !migrate_managed_setup_metadata_backend_with_value(&path, &mut state, Some("turso"))
                .unwrap()
        );
        let persisted = read_managed_setup_state(&path).unwrap().unwrap();
        assert_eq!(
            persisted.metadata_backend,
            Some(SetupMetadataBackend::Sqlite)
        );
    }

    #[test]
    fn legacy_metadata_backend_migration_uses_environment_value() {
        let dir = temp_dir("legacy-metadata-backend-environment");
        let path = managed_setup_state_path(&dir);
        let mut state = ManagedSetupState {
            version: LEGACY_SETUP_STATE_VERSION_2,
            metadata_backend: None,
            ..ManagedSetupState::default()
        };

        assert!(
            migrate_managed_setup_metadata_backend_with_value(&path, &mut state, Some("sqlite"))
                .unwrap()
        );
        assert_eq!(state.metadata_backend, Some(SetupMetadataBackend::Sqlite));
    }

    #[cfg(feature = "turso-metadata")]
    #[test]
    fn legacy_metadata_backend_migration_preserves_turso_environment_value() {
        let dir = temp_dir("legacy-metadata-backend-turso");
        let path = managed_setup_state_path(&dir);
        let mut state = ManagedSetupState {
            version: LEGACY_SETUP_STATE_VERSION_2,
            metadata_backend: None,
            ..ManagedSetupState::default()
        };

        assert!(
            migrate_managed_setup_metadata_backend_with_value(&path, &mut state, Some("turso"))
                .unwrap()
        );
        assert_eq!(state.metadata_backend, Some(SetupMetadataBackend::Turso));
    }

    #[test]
    fn derived_runtime_node_enrollment_path_resolves_without_doubling_data_dir_prefix() {
        // Regression test for the v1 failure mode. Version 2 and later no longer persist this
        // derivable path, but its fixed relative form must still resolve exactly once against
        // `data_dir`.
        let relative_data_dir = std::path::PathBuf::from("./data/server-node");
        let stored = runtime_node_enrollment_relative_path()
            .display()
            .to_string();
        let resolved = resolve_materialized_path(&relative_data_dir, &stored).unwrap();
        let expected =
            normalize_non_traversing_path(&runtime_node_enrollment_path(&relative_data_dir));
        assert_eq!(resolved, expected);
        assert!(
            !resolved
                .display()
                .to_string()
                .contains("data/server-node/data/server-node"),
            "enrollment path doubled the data_dir prefix: {}",
            resolved.display()
        );
    }

    #[test]
    fn relative_enrollment_data_dir_materializes_tls_paths_exactly_once() {
        let relative_data_dir = PathBuf::from("target")
            .join(format!("ironmesh-relative-enrollment-{}", Uuid::now_v7()));
        let absolute_data_dir = std::env::current_dir().unwrap().join(&relative_data_dir);
        let _ = std::fs::remove_dir_all(&absolute_data_dir);
        let bind_addr = "127.0.0.1:28443".parse::<SocketAddr>().unwrap();
        let package = test_node_enrollment_package(&relative_data_dir, bind_addr);

        let config = ServerNodeConfig::from_enrollment(package).unwrap();
        let internal_tls = config.internal_tls.as_ref().unwrap();
        let expected_cert_path = relative_data_dir.join(MANAGED_INTERNAL_CERT_PATH);

        assert_eq!(internal_tls.cert_path, expected_cert_path);
        assert!(internal_tls.cert_path.exists());
        assert!(
            !internal_tls
                .cert_path
                .display()
                .to_string()
                .contains("managed/runtime/data"),
            "managed TLS path was unexpectedly prefixed more than once: {}",
            internal_tls.cert_path.display()
        );

        let _ = std::fs::remove_dir_all(absolute_data_dir);
    }

    #[test]
    fn legacy_recovery_state_self_heals_and_migrates_without_config_changes() {
        let setup_dir = temp_dir("legacy-recovery-migration");
        let config =
            managed_startup_bootstrap_config(setup_dir.clone(), "127.0.0.1:28444".parse().unwrap())
                .unwrap();
        let relative_runtime_data_dir = PathBuf::from("target").join(format!(
            "ironmesh-legacy-managed-runtime-{}",
            Uuid::now_v7()
        ));
        let absolute_runtime_data_dir = std::env::current_dir()
            .unwrap()
            .join(&relative_runtime_data_dir);
        let _ = std::fs::remove_dir_all(&absolute_runtime_data_dir);

        let package = test_node_enrollment_package(&relative_runtime_data_dir, config.bind_addr);
        let cluster_id = package.bootstrap.cluster_id;
        let node_id = package.bootstrap.node_id;
        let enrollment_path = runtime_node_enrollment_path(&config.data_dir);
        package.write_to_path(&enrollment_path).unwrap();
        assert!(
            !absolute_runtime_data_dir
                .join(MANAGED_INTERNAL_CERT_PATH)
                .exists()
        );

        let legacy_state = ManagedSetupState {
            version: LEGACY_SETUP_STATE_VERSION,
            state: SetupLifecycleState::Recovery,
            cluster_id: Some(cluster_id),
            node_id: Some(node_id),
            runtime_node_enrollment_path: Some(enrollment_path.display().to_string()),
            metadata_backend: None,
            ..ManagedSetupState::default()
        };
        let legacy_payload = serde_json::to_string_pretty(&legacy_state).unwrap();
        assert!(!legacy_payload.contains("runtime_data_dir"));
        assert!(!legacy_payload.contains("recovery_reason"));
        assert!(!legacy_payload.contains("metadata_backend"));
        std::fs::write(&config.state_path, legacy_payload).unwrap();

        let startup_mode = load_managed_startup_mode(config.clone()).unwrap();
        let StartupMode::Runtime(runtime) = startup_mode else {
            panic!("valid embedded enrollment material should self-heal recovery");
        };
        assert_eq!(runtime.data_dir, absolute_runtime_data_dir);
        assert!(matches!(
            runtime.metadata_backend,
            MetadataBackendKind::Sqlite
        ));
        assert_eq!(
            runtime.internal_tls.as_ref().unwrap().cert_path,
            absolute_runtime_data_dir.join(MANAGED_INTERNAL_CERT_PATH)
        );
        assert!(
            absolute_runtime_data_dir
                .join(MANAGED_INTERNAL_CERT_PATH)
                .exists()
        );

        let migrated = read_managed_setup_state(&config.state_path)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.version, SETUP_STATE_VERSION);
        assert_eq!(migrated.state, SetupLifecycleState::Online);
        assert!(migrated.runtime_node_enrollment_path.is_none());
        assert_eq!(
            migrated.runtime_data_dir.as_deref(),
            Some(absolute_runtime_data_dir.to_string_lossy().as_ref())
        );
        assert!(migrated.recovery_reason.is_none());
        assert_eq!(
            migrated.metadata_backend,
            Some(SetupMetadataBackend::Sqlite)
        );

        let migrated_package = NodeEnrollmentPackage::from_path(&enrollment_path).unwrap();
        assert_eq!(
            migrated_package.bootstrap.data_dir,
            absolute_runtime_data_dir.display().to_string()
        );
        assert_eq!(
            migrated_package
                .bootstrap
                .internal_tls
                .as_ref()
                .unwrap()
                .cert_path,
            MANAGED_INTERNAL_CERT_PATH
        );

        let state_after_migration = std::fs::read_to_string(&config.state_path).unwrap();
        let restarted = load_managed_startup_mode(config.clone()).unwrap();
        assert!(matches!(restarted, StartupMode::Runtime(_)));
        assert_eq!(
            std::fs::read_to_string(&config.state_path).unwrap(),
            state_after_migration,
            "an idempotent restart must not rewrite managed setup state"
        );

        let _ = std::fs::remove_dir_all(absolute_runtime_data_dir);
    }

    #[test]
    fn resolve_materialized_path_rejects_parent_traversal() {
        let data_dir = std::path::PathBuf::from("./data/server-node");

        assert!(resolve_materialized_path(&data_dir, "../node-enrollment.json").is_err());
    }

    #[test]
    fn recovery_preserves_persisted_metadata_backend() {
        let managed = ManagedSetupState {
            state: SetupLifecycleState::Recovery,
            metadata_backend: Some(SetupMetadataBackend::Sqlite),
            ..ManagedSetupState::default()
        };

        assert_eq!(
            resolve_setup_metadata_backend(None, &managed).unwrap(),
            SetupMetadataBackend::Sqlite
        );
        assert_eq!(
            resolve_setup_metadata_backend(Some(SetupMetadataBackend::Sqlite), &managed).unwrap(),
            SetupMetadataBackend::Sqlite
        );
        assert!(
            resolve_setup_metadata_backend(Some(SetupMetadataBackend::Turso), &managed).is_err()
        );
    }

    #[test]
    fn managed_startup_bootstrap_config_rejects_parent_traversal_data_dir() {
        let bind_addr = "127.0.0.1:18443".parse::<SocketAddr>().unwrap();

        assert!(
            managed_startup_bootstrap_config(std::path::PathBuf::from("../data"), bind_addr)
                .is_err()
        );
    }

    #[tokio::test]
    async fn setup_runtime_transition_is_deferred_long_enough_for_response_flush() {
        let dir = temp_dir("deferred-setup-transition");
        let config = test_cluster_config_without_internal_tls(
            dir.join("data"),
            "127.0.0.1:28080".parse::<SocketAddr>().unwrap(),
        );
        let expected_bind_addr = config.bind_addr;
        let (tx, mut rx) = mpsc::channel(1);
        let permit = tx.reserve_owned().await.unwrap();

        spawn_setup_runtime_transition(permit, config);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), rx.recv())
                .await
                .is_err(),
            "runtime transition should not fire immediately"
        );

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("runtime transition should arrive after delay")
            .expect("runtime transition channel should stay open");
        assert_eq!(completion.config.bind_addr, expected_bind_addr);
    }

    #[test]
    fn self_managed_cluster_enrollment_is_valid() {
        let dir = temp_dir("start-cluster");
        let node_id = NodeId::new_v4();
        let cluster_id = Uuid::now_v7();
        let bootstrap = TransportNodeBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            node_id,
            mode: NodeBootstrapMode::Cluster,
            data_dir: dir.display().to_string(),
            bind_addr: "0.0.0.0:8443".to_string(),
            public_url: Some("https://node-a.local:8443".to_string()),
            labels: default_setup_labels(),
            public_tls: Some(BootstrapServerTlsFiles {
                cert_path: "managed/runtime/public/public.pem".to_string(),
                key_path: "managed/runtime/public/public.key".to_string(),
            }),
            public_ca_cert_path: Some("managed/runtime/public/public-ca.pem".to_string()),
            public_peer_api_enabled: false,
            internal_bind_addr: Some("0.0.0.0:18443".to_string()),
            internal_url: Some("https://node-a.local:18443".to_string()),
            internal_tls: Some(BootstrapTlsFiles {
                ca_cert_path: "managed/runtime/internal/cluster-ca.pem".to_string(),
                cert_path: "managed/runtime/internal/node.pem".to_string(),
                key_path: "managed/runtime/internal/node.key".to_string(),
            }),
            rendezvous_urls: vec!["https://node-a.local:9443".to_string()],
            rendezvous_mtls_required: true,
            direct_endpoints: vec![
                BootstrapEndpoint {
                    url: "https://node-a.local:8443".to_string(),
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(node_id),
                },
                BootstrapEndpoint {
                    url: "https://node-a.local:18443".to_string(),
                    usage: Some(BootstrapEndpointUse::PeerApi),
                    node_id: Some(node_id),
                },
            ],
            relay_mode: RelayMode::Fallback,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            enrollment_issuer_url: Some("https://node-a.local:8443".to_string()),
        };

        let package = issue_self_managed_cluster_artifacts(bootstrap)
            .unwrap()
            .package;
        package.validate().unwrap();
        assert!(package.public_tls_material.is_some());
        assert!(package.internal_tls_material.is_some());
        assert!(package.bootstrap.trust_roots.cluster_ca_pem.is_some());
        assert_eq!(
            package.bootstrap.rendezvous_urls,
            vec!["https://node-a.local:9443".to_string()]
        );
        assert!(package.bootstrap.trust_roots.rendezvous_ca_pem.is_some());
    }

    #[tokio::test]
    async fn import_node_enrollment_package_uses_node_local_admin_password() {
        let dir = temp_dir("import-node-local-admin-password");
        let data_dir = dir.join("data");
        let bind_addr = "127.0.0.1:18443".parse::<SocketAddr>().unwrap();
        let config = managed_startup_bootstrap_config(data_dir.clone(), bind_addr).unwrap();
        let (completion_tx, mut completion_rx) = mpsc::channel(1);
        let state = SetupServerState {
            config,
            managed_state: Arc::new(Mutex::new(ManagedSetupState::default())),
            completion_tx,
        };
        let package = test_node_enrollment_package(&data_dir, bind_addr);
        // Generated rather than literals so static analysis doesn't mistake these test-only
        // values for hard-coded credentials (CodeQL rust/hard-coded-cryptographic-value).
        let issuer_admin_password = format!("test-issuer-admin-password-{}", Uuid::now_v7());
        let node_admin_password = format!("test-node-admin-password-{}", Uuid::now_v7());

        let response = import_node_enrollment_package(
            State(state.clone()),
            Json(SetupImportEnrollmentRequest {
                admin_password: node_admin_password.clone(),
                package_json: package.to_json_pretty().unwrap(),
                metadata_backend: Some(SetupMetadataBackend::Sqlite),
                telemetry_enabled: true,
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CREATED);

        let managed = state.managed_state.lock().await.clone();
        let managed_hash = managed
            .admin_password_hash
            .as_deref()
            .expect("managed state should persist an admin password hash");
        assert!(crate::password_hash_matches(
            managed_hash,
            &node_admin_password
        ));
        assert!(!crate::password_hash_matches(
            managed_hash,
            &issuer_admin_password
        ));

        let completion = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .expect("runtime transition should be scheduled")
            .expect("runtime transition channel should stay open");
        assert!(matches!(
            completion.config.metadata_backend,
            MetadataBackendKind::Sqlite
        ));
        let runtime_hash = completion
            .config
            .admin_password_hash
            .as_deref()
            .expect("runtime config should receive an admin password hash");
        assert!(crate::password_hash_matches(
            runtime_hash,
            &node_admin_password
        ));
        assert!(!crate::password_hash_matches(
            runtime_hash,
            &issuer_admin_password
        ));
    }

    #[test]
    fn setup_start_cluster_request_defaults_telemetry_enabled_when_omitted() {
        // Non-UI callers (or older clients) that omit the field entirely must still land on the
        // opt-out default, per doc Section 4.4.
        let request: SetupStartClusterRequest = serde_json::from_str(
            r#"{"admin_password":"a-very-strong-password","public_origin":"https://node-a.local:8443"}"#,
        )
        .unwrap();
        assert!(request.telemetry_enabled);
    }

    #[test]
    fn setup_start_cluster_request_respects_explicit_telemetry_enabled() {
        let request: SetupStartClusterRequest = serde_json::from_str(
            r#"{"admin_password":"a-very-strong-password","public_origin":"https://node-a.local:8443","telemetry_enabled":false}"#,
        )
        .unwrap();
        assert!(!request.telemetry_enabled);
    }

    #[test]
    fn setup_import_enrollment_request_defaults_telemetry_enabled_when_omitted() {
        let request: SetupImportEnrollmentRequest = serde_json::from_str(
            r#"{"admin_password":"a-very-strong-password","package_json":"{}"}"#,
        )
        .unwrap();
        assert!(request.telemetry_enabled);
    }

    #[tokio::test]
    async fn start_new_cluster_applies_setup_telemetry_choice() {
        let dir = temp_dir("start-cluster-telemetry");
        let data_dir = dir.join("data");
        let bind_addr = "127.0.0.1:18443".parse::<SocketAddr>().unwrap();
        let config = managed_startup_bootstrap_config(data_dir.clone(), bind_addr).unwrap();
        let (completion_tx, mut completion_rx) = mpsc::channel(1);
        let state = SetupServerState {
            config,
            managed_state: Arc::new(Mutex::new(ManagedSetupState::default())),
            completion_tx,
        };

        // Generated rather than a literal so static analysis doesn't mistake this test-only
        // value for a hard-coded credential (CodeQL rust/hard-coded-cryptographic-value).
        let admin_password = format!("test-admin-password-{}", Uuid::now_v7());
        let response = start_new_cluster(
            State(state.clone()),
            Json(SetupStartClusterRequest {
                admin_password,
                public_origin: "https://node-a.local:18443".to_string(),
                metadata_backend: None,
                telemetry_enabled: true,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);

        let completion = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .expect("runtime transition should be scheduled")
            .expect("runtime transition channel should stay open");
        assert_eq!(
            SetupMetadataBackend::from(completion.config.metadata_backend),
            default_setup_metadata_backend()
        );
        assert_eq!(
            state.managed_state.lock().await.metadata_backend,
            Some(default_setup_metadata_backend())
        );

        // The setup-time choice must land in exactly the same persisted override the admin
        // toggle uses, so a fresh load of the runtime sees it as an explicit override, not just
        // the (also-true) env default.
        let runtime = reliability_telemetry::ReliabilityTelemetryRuntime::load(&data_dir);
        assert!(runtime.effective_enabled());
        assert_eq!(runtime.enabled_source(), "admin_override");
    }

    #[tokio::test]
    async fn import_node_enrollment_package_applies_disabled_setup_telemetry_choice() {
        let dir = temp_dir("import-telemetry-disabled");
        let data_dir = dir.join("data");
        let bind_addr = "127.0.0.1:18443".parse::<SocketAddr>().unwrap();
        let config = managed_startup_bootstrap_config(data_dir.clone(), bind_addr).unwrap();
        let (completion_tx, mut completion_rx) = mpsc::channel(1);
        let state = SetupServerState {
            config,
            managed_state: Arc::new(Mutex::new(ManagedSetupState::default())),
            completion_tx,
        };
        let package = test_node_enrollment_package(&data_dir, bind_addr);

        // Generated rather than a literal so static analysis doesn't mistake this test-only
        // value for a hard-coded credential (CodeQL rust/hard-coded-cryptographic-value).
        let admin_password = format!("test-admin-password-{}", Uuid::now_v7());
        let response = import_node_enrollment_package(
            State(state.clone()),
            Json(SetupImportEnrollmentRequest {
                admin_password,
                package_json: package.to_json_pretty().unwrap(),
                metadata_backend: Some(SetupMetadataBackend::Sqlite),
                telemetry_enabled: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);

        let completion = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .expect("runtime transition should be scheduled")
            .expect("runtime transition channel should stay open");
        assert!(matches!(
            completion.config.metadata_backend,
            MetadataBackendKind::Sqlite
        ));

        let runtime = reliability_telemetry::ReliabilityTelemetryRuntime::load(&data_dir);
        assert!(!runtime.effective_enabled());
        assert_eq!(runtime.enabled_source(), "admin_override");
    }

    #[test]
    fn expired_runtime_enrollment_requires_rejoin() {
        let dir = temp_dir("expired-runtime-enrollment");
        let node_id = NodeId::new_v4();
        let cluster_id = Uuid::now_v7();
        let bootstrap = TransportNodeBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            node_id,
            mode: NodeBootstrapMode::Cluster,
            data_dir: dir.display().to_string(),
            bind_addr: "0.0.0.0:8443".to_string(),
            public_url: Some("https://node-a.local:8443".to_string()),
            labels: default_setup_labels(),
            public_tls: Some(BootstrapServerTlsFiles {
                cert_path: "managed/runtime/public/public.pem".to_string(),
                key_path: "managed/runtime/public/public.key".to_string(),
            }),
            public_ca_cert_path: Some("managed/runtime/public/public-ca.pem".to_string()),
            public_peer_api_enabled: false,
            internal_bind_addr: Some("0.0.0.0:18443".to_string()),
            internal_url: Some("https://node-a.local:18443".to_string()),
            internal_tls: Some(BootstrapTlsFiles {
                ca_cert_path: "managed/runtime/internal/cluster-ca.pem".to_string(),
                cert_path: "managed/runtime/internal/node.pem".to_string(),
                key_path: "managed/runtime/internal/node.key".to_string(),
            }),
            rendezvous_urls: vec!["https://node-a.local:9443".to_string()],
            rendezvous_mtls_required: true,
            direct_endpoints: vec![
                BootstrapEndpoint {
                    url: "https://node-a.local:8443".to_string(),
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(node_id),
                },
                BootstrapEndpoint {
                    url: "https://node-a.local:18443".to_string(),
                    usage: Some(BootstrapEndpointUse::PeerApi),
                    node_id: Some(node_id),
                },
            ],
            relay_mode: RelayMode::Fallback,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            enrollment_issuer_url: Some("https://node-a.local:8443".to_string()),
        };

        let mut artifacts = issue_self_managed_cluster_artifacts(bootstrap).unwrap();
        let now = unix_ts();
        let expired_policy = NodeTlsIssuePolicy {
            issued_at_unix: now.saturating_sub(7100),
            not_before_unix: now.saturating_sub(7200),
            not_after_unix: now.saturating_sub(3600),
            renew_after_unix: now.saturating_sub(3900),
        };
        artifacts.package.internal_tls_material = Some(
            issue_internal_node_tls_material_from_ca(
                &artifacts.package.bootstrap,
                &artifacts.ca_cert_pem,
                &artifacts.ca_key_pem,
                expired_policy,
            )
            .unwrap(),
        );

        let package_path = dir
            .join("managed")
            .join("runtime")
            .join("node-enrollment.json");
        artifacts.package.write_to_path(&package_path).unwrap();

        let config = ServerNodeConfig::from_enrollment_path(&package_path).unwrap();
        assert_eq!(
            runtime_enrollment_recovery_reason(&config),
            Some(ManagedRecoveryReasonCode::CertificateExpired)
        );
    }

    #[test]
    fn online_managed_startup_with_expired_runtime_enrollment_falls_back_to_recovery_setup() {
        let dir = temp_dir("expired-runtime-startup-recovery");
        let runtime_enrollment_path = runtime_node_enrollment_path(&dir);
        let node_id = NodeId::new_v4();
        let cluster_id = Uuid::now_v7();
        let bootstrap = TransportNodeBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            node_id,
            mode: NodeBootstrapMode::Cluster,
            data_dir: dir.display().to_string(),
            bind_addr: "0.0.0.0:8443".to_string(),
            public_url: Some("https://node-a.local:8443".to_string()),
            labels: default_setup_labels(),
            public_tls: Some(BootstrapServerTlsFiles {
                cert_path: "managed/runtime/public/public.pem".to_string(),
                key_path: "managed/runtime/public/public.key".to_string(),
            }),
            public_ca_cert_path: Some("managed/runtime/public/public-ca.pem".to_string()),
            public_peer_api_enabled: false,
            internal_bind_addr: Some("0.0.0.0:18443".to_string()),
            internal_url: Some("https://node-a.local:18443".to_string()),
            internal_tls: Some(BootstrapTlsFiles {
                ca_cert_path: "managed/runtime/internal/cluster-ca.pem".to_string(),
                cert_path: "managed/runtime/internal/node.pem".to_string(),
                key_path: "managed/runtime/internal/node.key".to_string(),
            }),
            rendezvous_urls: vec!["https://node-a.local:9443".to_string()],
            rendezvous_mtls_required: true,
            direct_endpoints: vec![
                BootstrapEndpoint {
                    url: "https://node-a.local:8443".to_string(),
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(node_id),
                },
                BootstrapEndpoint {
                    url: "https://node-a.local:18443".to_string(),
                    usage: Some(BootstrapEndpointUse::PeerApi),
                    node_id: Some(node_id),
                },
            ],
            relay_mode: RelayMode::Fallback,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            enrollment_issuer_url: Some("https://node-a.local:8443".to_string()),
        };

        let mut artifacts = issue_self_managed_cluster_artifacts(bootstrap).unwrap();
        let now = unix_ts();
        let expired_policy = NodeTlsIssuePolicy {
            issued_at_unix: now.saturating_sub(7100),
            not_before_unix: now.saturating_sub(7200),
            not_after_unix: now.saturating_sub(3600),
            renew_after_unix: now.saturating_sub(3900),
        };
        artifacts.package.internal_tls_material = Some(
            issue_internal_node_tls_material_from_ca(
                &artifacts.package.bootstrap,
                &artifacts.ca_cert_pem,
                &artifacts.ca_key_pem,
                expired_policy,
            )
            .unwrap(),
        );
        artifacts
            .package
            .write_to_path(&runtime_enrollment_path)
            .unwrap();

        let state_path = managed_setup_state_path(&dir);
        write_managed_setup_state(
            &state_path,
            &ManagedSetupState {
                state: SetupLifecycleState::Online,
                cluster_id: Some(cluster_id),
                node_id: Some(node_id),
                runtime_node_enrollment_path: Some(runtime_enrollment_path.display().to_string()),
                ..ManagedSetupState::default()
            },
        )
        .unwrap();

        let startup_mode = load_managed_startup_mode(SetupBootstrapConfig {
            data_dir: dir.clone(),
            bind_addr: "127.0.0.1:8443".parse().unwrap(),
            state_path: state_path.clone(),
            bootstrap_cert_path: bootstrap_setup_cert_path(&dir),
            bootstrap_key_path: bootstrap_setup_key_path(&dir),
        })
        .unwrap();

        assert!(matches!(startup_mode, StartupMode::Setup(_)));
        let restored = read_managed_setup_state(&state_path).unwrap().unwrap();
        assert_eq!(restored.state, SetupLifecycleState::Recovery);
        assert_eq!(restored.cluster_id, Some(cluster_id));
        assert_eq!(restored.node_id, Some(node_id));
        assert!(restored.pending_join_request.is_none());
        assert_eq!(
            restored.recovery_reason.as_ref().map(|reason| reason.code),
            Some(ManagedRecoveryReasonCode::CertificateExpired)
        );
        assert_eq!(
            restored.runtime_data_dir.as_deref(),
            Some(dir.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn apply_managed_rendezvous_config_enables_embedded_listener() {
        let dir = temp_dir("managed-rendezvous-config");
        let runtime_internal_dir = managed_setup_dir(&dir).join("runtime").join("internal");
        std::fs::create_dir_all(&runtime_internal_dir).unwrap();
        std::fs::write(runtime_internal_dir.join("cluster-ca.pem"), "cluster-ca").unwrap();
        let rendezvous_dir = managed_rendezvous_dir(&dir);
        std::fs::create_dir_all(&rendezvous_dir).unwrap();
        std::fs::write(rendezvous_dir.join("rendezvous.pem"), "cert").unwrap();
        std::fs::write(rendezvous_dir.join("rendezvous.key"), "key").unwrap();

        let managed_state = ManagedSetupState {
            managed_rendezvous_bind_addr: Some("0.0.0.0:9443".to_string()),
            managed_rendezvous_public_url: Some("https://node-a.local:9443".to_string()),
            ..ManagedSetupState::default()
        };
        let mut config = test_cluster_config_without_internal_tls(
            dir.join("data"),
            "127.0.0.1:28080".parse::<SocketAddr>().unwrap(),
        );

        apply_managed_rendezvous_config(&dir, &managed_state, &mut config);

        assert_eq!(
            config.rendezvous_urls,
            vec![
                format!("http://{}", config.bind_addr),
                "https://node-a.local:9443/".to_string()
            ]
        );
        assert!(config.rendezvous_registration_enabled);
        assert!(config.rendezvous_mtls_required);
        assert_eq!(
            config
                .managed_rendezvous
                .as_ref()
                .map(|cfg| cfg.public_url.as_str()),
            Some("https://node-a.local:9443")
        );
    }

    #[test]
    fn apply_managed_rendezvous_config_deduplicates_trailing_slash_variants() {
        let dir = temp_dir("managed-rendezvous-config-dedup");
        let runtime_internal_dir = managed_setup_dir(&dir).join("runtime").join("internal");
        std::fs::create_dir_all(&runtime_internal_dir).unwrap();
        std::fs::write(runtime_internal_dir.join("cluster-ca.pem"), "cluster-ca").unwrap();
        let rendezvous_dir = managed_rendezvous_dir(&dir);
        std::fs::create_dir_all(&rendezvous_dir).unwrap();
        std::fs::write(rendezvous_dir.join("rendezvous.pem"), "cert").unwrap();
        std::fs::write(rendezvous_dir.join("rendezvous.key"), "key").unwrap();

        let managed_state = ManagedSetupState {
            managed_rendezvous_bind_addr: Some("0.0.0.0:9443".to_string()),
            managed_rendezvous_public_url: Some("https://node-a.local:9443".to_string()),
            ..ManagedSetupState::default()
        };
        let mut config = test_cluster_config_without_internal_tls(
            dir.join("data"),
            "127.0.0.1:28080".parse::<SocketAddr>().unwrap(),
        );
        config
            .rendezvous_urls
            .push("https://node-a.local:9443/".to_string());

        apply_managed_rendezvous_config(&dir, &managed_state, &mut config);

        assert_eq!(
            config.rendezvous_urls,
            vec![
                format!("http://{}", config.bind_addr),
                "https://node-a.local:9443/".to_string()
            ]
        );
    }

    #[test]
    fn apply_managed_signer_paths_sets_runtime_ca_key_paths() {
        let dir = temp_dir("managed-signer");
        write_managed_signer_material(&dir, "ca-cert", "ca-key").unwrap();
        let mut config = test_cluster_config_without_internal_tls(
            dir.join("data"),
            "127.0.0.1:28080".parse::<SocketAddr>().unwrap(),
        );

        apply_managed_signer_paths(&dir, &mut config);

        assert_eq!(
            config.internal_ca_key_path.as_ref().map(|path| path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()),
            Some("cluster-ca.key".to_string())
        );
        assert_eq!(
            config.public_ca_key_path.as_ref().map(|path| path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()),
            Some("cluster-ca.key".to_string())
        );
    }

    #[test]
    fn managed_signer_backup_roundtrip_restores_signer_material() {
        let dir = temp_dir("managed-signer-backup");
        let cluster_id = ClusterId::new_v4();
        let source_node_id = NodeId::new_v4();
        let backup = export_managed_signer_backup(
            cluster_id,
            source_node_id,
            "cluster-ca-cert",
            "cluster-ca-key",
            "correct horse battery staple",
        )
        .unwrap();

        import_managed_signer_backup(
            &dir,
            &backup,
            "correct horse battery staple",
            Some(cluster_id),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(managed_signer_ca_cert_path(&dir)).unwrap(),
            "cluster-ca-cert"
        );
        assert_eq!(
            std::fs::read_to_string(managed_signer_ca_key_path(&dir)).unwrap(),
            "cluster-ca-key"
        );
    }

    #[test]
    fn managed_signer_backup_rejects_wrong_passphrase() {
        let dir = temp_dir("managed-signer-backup-passphrase");
        let backup = export_managed_signer_backup(
            ClusterId::new_v4(),
            NodeId::new_v4(),
            "cluster-ca-cert",
            "cluster-ca-key",
            "correct horse battery staple",
        )
        .unwrap();

        let err = import_managed_signer_backup(
            &dir,
            &backup,
            "wrong passphrase",
            Some(backup.cluster_id),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("failed decrypting managed signer backup")
        );
    }

    #[test]
    fn managed_rendezvous_failover_roundtrip_restores_material_and_state() {
        let dir = temp_dir("managed-rendezvous-failover");
        let cluster_id = ClusterId::new_v4();
        let source_node_id = NodeId::new_v4();
        let target_node_id = NodeId::new_v4();
        let package =
            export_managed_rendezvous_failover_package(ManagedRendezvousFailoverExportParams {
                cluster_id,
                source_node_id,
                target_node_id: Some(target_node_id),
                public_url: "https://rendezvous.example:9443",
                deployment_target: ManagedRendezvousFailoverDeploymentTarget::EmbeddedNode,
                client_ca_cert_pem: "cluster-ca-cert",
                cert_pem: "rendezvous-cert",
                key_pem: "rendezvous-key",
                passphrase: "correct horse battery staple",
            })
            .unwrap();

        import_managed_rendezvous_failover_package(
            &dir,
            &package,
            "correct horse battery staple",
            "0.0.0.0:9443".parse::<SocketAddr>().unwrap(),
            Some(cluster_id),
            Some(target_node_id),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(managed_runtime_internal_ca_cert_path(&dir)).unwrap(),
            "cluster-ca-cert"
        );
        assert_eq!(
            std::fs::read_to_string(managed_rendezvous_cert_path(&dir)).unwrap(),
            "rendezvous-cert"
        );
        assert_eq!(
            std::fs::read_to_string(managed_rendezvous_key_path(&dir)).unwrap(),
            "rendezvous-key"
        );

        let restored = read_managed_setup_state(&managed_setup_state_path(&dir))
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.managed_rendezvous_public_url.as_deref(),
            Some("https://rendezvous.example:9443")
        );
        assert_eq!(
            restored.managed_rendezvous_bind_addr.as_deref(),
            Some("0.0.0.0:9443")
        );
        assert_eq!(restored.cluster_id, Some(cluster_id));
        assert_eq!(restored.node_id, Some(target_node_id));
    }

    #[test]
    fn managed_rendezvous_failover_can_mark_standalone_service_exports() {
        let package =
            export_managed_rendezvous_failover_package(ManagedRendezvousFailoverExportParams {
                cluster_id: ClusterId::new_v4(),
                source_node_id: NodeId::new_v4(),
                target_node_id: None,
                public_url: "https://rendezvous.example:9443",
                deployment_target: ManagedRendezvousFailoverDeploymentTarget::StandaloneService,
                client_ca_cert_pem: "cluster-ca-cert",
                cert_pem: "rendezvous-cert",
                key_pem: "rendezvous-key",
                passphrase: "correct horse battery staple",
            })
            .unwrap();

        assert_eq!(
            package.deployment_target,
            ManagedRendezvousFailoverDeploymentTarget::StandaloneService
        );
        assert!(package.includes_cluster_ca_cert);
        assert_eq!(package.target_node_id, None);
    }

    #[test]
    fn managed_rendezvous_failover_rejects_wrong_target_node() {
        let dir = temp_dir("managed-rendezvous-failover-target");
        let cluster_id = ClusterId::new_v4();
        let package =
            export_managed_rendezvous_failover_package(ManagedRendezvousFailoverExportParams {
                cluster_id,
                source_node_id: NodeId::new_v4(),
                target_node_id: Some(NodeId::new_v4()),
                public_url: "https://rendezvous.example:9443",
                deployment_target: ManagedRendezvousFailoverDeploymentTarget::EmbeddedNode,
                client_ca_cert_pem: "cluster-ca-cert",
                cert_pem: "rendezvous-cert",
                key_pem: "rendezvous-key",
                passphrase: "correct horse battery staple",
            })
            .unwrap();

        let err = import_managed_rendezvous_failover_package(
            &dir,
            &package,
            "correct horse battery staple",
            "0.0.0.0:9443".parse::<SocketAddr>().unwrap(),
            Some(cluster_id),
            Some(NodeId::new_v4()),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("managed rendezvous failover targets node")
        );
    }
}
