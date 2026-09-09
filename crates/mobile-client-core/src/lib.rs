//! Platform-neutral lifecycle owner for the iOS and Android clients.
//!
//! Platform adapters provide persistence callbacks and translate their FFI/UI
//! types. Connection affinity, reusable sessions, and the single loopback Web
//! UI lifecycle are owned here so neither adapter needs its own locks or pools.

use anyhow::{Context, Result};
use client_sdk::{
    ClientIdentityMaterial, ClientNode, ConnectionBootstrap, IronMeshClient, ManagedClientOptions,
    ManagedIronMeshClient, TitleLatencyMonitor, TitleLatencyProbeConfig, TitleLatencyProbeStatus,
};
use common::logging::LogBuffer;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;
use uuid::Uuid;
use web_ui_backend::{EmbeddedWebUiSessionAuthorization, WebUiBootstrapPersistence, WebUiConfig};

/// Stable identity of the transport configuration used by one mobile session.
///
/// Authenticated contact-list updates and renewable certificate metadata do not
/// change this identity. Durable key material, trust, and bootstrap routes do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileConnectionAffinity {
    connection_identity: String,
    client_identity: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct MobileClientInput {
    connection_input: String,
    server_ca_pem: Option<String>,
    client_identity_json: Option<String>,
}

impl MobileClientInput {
    fn new(
        connection_input: impl AsRef<str>,
        server_ca_pem: Option<impl AsRef<str>>,
        client_identity_json: Option<impl AsRef<str>>,
    ) -> Self {
        Self {
            connection_input: connection_input.as_ref().trim().to_string(),
            server_ca_pem: normalize_optional(server_ca_pem),
            client_identity_json: normalize_optional(client_identity_json),
        }
    }
}

/// Parsed and normalized configuration shared by both mobile adapters.
#[derive(Clone)]
pub struct MobileClientConfiguration {
    normalized_connection_input: String,
    effective_bootstrap: ConnectionBootstrap,
    client_identity: Option<ClientIdentityMaterial>,
    affinity: MobileConnectionAffinity,
}

impl MobileClientConfiguration {
    pub fn new(
        connection_input: impl AsRef<str>,
        server_ca_pem: Option<impl AsRef<str>>,
        client_identity_json: Option<impl AsRef<str>>,
    ) -> Result<Self> {
        let input = MobileClientInput::new(connection_input, server_ca_pem, client_identity_json);
        Self::from_input(&input)
    }

    fn from_input(input: &MobileClientInput) -> Result<Self> {
        let connection_input = input.connection_input.as_str();
        anyhow::ensure!(
            !connection_input.is_empty(),
            "mobile client requires a non-empty connection bootstrap"
        );

        let mut bootstrap = ConnectionBootstrap::from_json_str(connection_input)
            .context("failed to parse mobile connection bootstrap JSON")?;
        let normalized_connection_input = bootstrap
            .to_json_pretty()
            .context("failed to normalize mobile connection bootstrap JSON")?;
        if let Some(server_ca_pem) = input.server_ca_pem.as_ref() {
            bootstrap.trust_roots.public_api_ca_pem = Some(server_ca_pem.clone());
        }

        let client_identity = input
            .client_identity_json
            .as_deref()
            .map(ClientIdentityMaterial::from_json_str)
            .transpose()
            .context("failed to parse mobile client identity JSON")?;

        let mut affinity_bootstrap = bootstrap.clone();
        // A managed session applies authenticated contact-list updates in place.
        // Persisting that update must not create a competing session.
        affinity_bootstrap.rendezvous_contact_list = None;
        let connection_identity = affinity_bootstrap
            .to_json_pretty()
            .context("failed to normalize mobile connection affinity")?;
        let client_identity = client_identity.map(|identity| {
            let fingerprint = client_identity_affinity(&identity);
            (identity, fingerprint)
        });

        Ok(Self {
            normalized_connection_input,
            effective_bootstrap: bootstrap,
            affinity: MobileConnectionAffinity {
                connection_identity,
                client_identity: client_identity
                    .as_ref()
                    .map(|(_, fingerprint)| fingerprint.clone()),
            },
            client_identity: client_identity.map(|(identity, _)| identity),
        })
    }

    pub fn normalized_connection_input(&self) -> &str {
        &self.normalized_connection_input
    }

    pub fn effective_bootstrap(&self) -> &ConnectionBootstrap {
        &self.effective_bootstrap
    }

    pub fn client_identity(&self) -> Option<&ClientIdentityMaterial> {
        self.client_identity.as_ref()
    }

    pub fn affinity(&self) -> &MobileConnectionAffinity {
        &self.affinity
    }
}

fn normalize_optional<T>(value: Option<T>) -> Option<String>
where
    T: AsRef<str>,
{
    value.and_then(|value| {
        let value = value.as_ref().trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn client_identity_affinity(identity: &ClientIdentityMaterial) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        identity.cluster_id.to_string(),
        identity.device_id.to_string(),
        identity.private_key_pem.clone(),
        identity.public_key_pem.clone(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

type PersistIdentityFn = dyn Fn(&ClientIdentityMaterial) -> Result<()> + Send + Sync + 'static;

/// Optional platform persistence hook. The Core remains the owner of deciding
/// when an identity changed; the adapter only stores the supplied value.
#[derive(Clone)]
pub struct MobileIdentityPersistence {
    source: &'static str,
    persist: Arc<PersistIdentityFn>,
}

impl MobileIdentityPersistence {
    pub fn new<F>(source: &'static str, persist: F) -> Self
    where
        F: Fn(&ClientIdentityMaterial) -> Result<()> + Send + Sync + 'static,
    {
        Self {
            source,
            persist: Arc::new(persist),
        }
    }
}

/// Process-wide options supplied once by the thin platform adapter.
#[derive(Clone)]
pub struct MobileClientOptions {
    pub managed_client: ManagedClientOptions,
    pub identity_persistence: Option<MobileIdentityPersistence>,
    pub web_ui_service_name: String,
    pub web_ui_connection_name: String,
    pub web_ui_bootstrap_persistence: Option<WebUiBootstrapPersistence>,
    pub web_ui_log_buffer: Option<Arc<LogBuffer>>,
}

impl MobileClientOptions {
    pub fn new(platform: impl AsRef<str>) -> Self {
        let platform = platform.as_ref().trim().to_ascii_lowercase();
        let platform = if platform.is_empty() {
            "mobile".to_string()
        } else {
            platform
        };
        Self {
            managed_client: ManagedClientOptions::default(),
            identity_persistence: None,
            web_ui_service_name: format!("ironmesh-{platform}"),
            web_ui_connection_name: format!("{platform} web ui"),
            web_ui_bootstrap_persistence: None,
            web_ui_log_buffer: None,
        }
    }
}

struct MobileClientSessionInner {
    id: Uuid,
    runtime: Arc<Runtime>,
    configuration: MobileClientConfiguration,
    client: IronMeshClient,
    client_node: ClientNode,
    managed_client: Option<ManagedIronMeshClient>,
    client_identity: Option<ClientIdentityMaterial>,
}

/// Cloneable lease on an immutable, configuration-affine Rust client.
///
/// Replacing the process's active configuration never invalidates a lease that
/// is already serving an operation. This is the safety property previously
/// implemented with platform locks and FFI handle pools.
#[derive(Clone)]
pub struct MobileClientSession {
    inner: Arc<MobileClientSessionInner>,
}

impl MobileClientSession {
    pub fn id(&self) -> Uuid {
        self.inner.id
    }

    pub fn affinity(&self) -> &MobileConnectionAffinity {
        self.inner.configuration.affinity()
    }

    pub fn configuration(&self) -> &MobileClientConfiguration {
        &self.inner.configuration
    }

    pub fn client(&self, connection_name: impl Into<String>) -> IronMeshClient {
        self.inner
            .client
            .clone()
            .with_connection_name(connection_name.into())
    }

    pub fn base_client(&self) -> IronMeshClient {
        self.inner.client.clone()
    }

    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        self.inner.runtime.block_on(future)
    }

    pub fn client_node(&self, connection_name: impl Into<String>) -> ClientNode {
        self.inner
            .client_node
            .clone()
            .with_connection_name(connection_name)
    }

    pub fn base_client_node(&self) -> ClientNode {
        self.inner.client_node.clone()
    }

    pub fn managed_client(&self) -> Option<ManagedIronMeshClient> {
        self.inner.managed_client.clone()
    }

    pub fn client_identity(&self) -> Option<ClientIdentityMaterial> {
        self.inner.client_identity.clone()
    }

    pub fn take_client_identity_update(&self) -> Option<ClientIdentityMaterial> {
        self.inner
            .managed_client
            .as_ref()
            .and_then(ManagedIronMeshClient::take_identity_update)
    }

    pub fn take_connection_bootstrap_update(&self) -> Option<ConnectionBootstrap> {
        self.inner
            .managed_client
            .as_ref()
            .and_then(ManagedIronMeshClient::take_connection_bootstrap_update)
    }
}

struct ActiveConnection {
    affinity: MobileConnectionAffinity,
    source_input: Option<MobileClientInput>,
    session: MobileClientSession,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileWebUiSurface {
    WebUi,
    GalleryMap,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileWebUiPhase {
    Idle,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileWebUiFailureStage {
    Start,
    Server,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileWebUiRecovery {
    RetryStartOrAbort,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileWebUiFailure {
    pub stage: MobileWebUiFailureStage,
    pub message: String,
    pub recovery: MobileWebUiRecovery,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileWebUiSession {
    pub session_id: Uuid,
    pub surface: MobileWebUiSurface,
    pub url: String,
    pub authorization: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileWebUiState {
    pub revision: u64,
    pub phase: MobileWebUiPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<MobileWebUiSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<MobileWebUiSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<MobileWebUiFailure>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MobileWebUiCommandDisposition {
    Applied,
    Reused,
    Superseded,
    Noop,
}

/// Result of every Web UI command. Adapters apply snapshots monotonically by
/// `state.revision`; out-of-order FFI completions therefore cannot resurrect a
/// stopped or superseded native presentation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MobileWebUiCommandResult {
    pub disposition: MobileWebUiCommandDisposition,
    pub state: MobileWebUiState,
}

impl MobileWebUiCommandResult {
    /// Converts a successful start command into the session owned by that
    /// request. A superseded command must never expose another request's
    /// currently active authorization token through a platform adapter.
    pub fn into_started_session(
        self,
        expected_surface: MobileWebUiSurface,
    ) -> Result<MobileWebUiSession> {
        if self.state.phase == MobileWebUiPhase::Failed {
            let message = self
                .state
                .failure
                .as_ref()
                .map(|failure| failure.message.as_str())
                .unwrap_or("embedded Web UI start failed");
            anyhow::bail!(message.to_string());
        }
        anyhow::ensure!(
            matches!(
                self.disposition,
                MobileWebUiCommandDisposition::Applied | MobileWebUiCommandDisposition::Reused
            ),
            "embedded Web UI start was superseded or cancelled"
        );
        anyhow::ensure!(
            self.state.phase == MobileWebUiPhase::Running,
            "embedded Web UI start did not enter the running state"
        );
        let session = self
            .state
            .session
            .context("embedded Web UI start did not publish a session")?;
        anyhow::ensure!(
            session.surface == expected_surface,
            "embedded Web UI start returned a session for another surface"
        );
        Ok(session)
    }
}

#[derive(Clone)]
struct WebUiIntent {
    request_id: u64,
    surface: MobileWebUiSurface,
    affinity: MobileConnectionAffinity,
}

struct ActiveWebUi {
    request_id: u64,
    affinity: MobileConnectionAffinity,
    session: MobileWebUiSession,
    task: JoinHandle<()>,
    completion: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Copy)]
enum WebUiTransition {
    Starting {
        request_id: u64,
        surface: MobileWebUiSurface,
    },
    Stopping {
        request_id: u64,
        surface: Option<MobileWebUiSurface>,
    },
}

impl WebUiTransition {
    fn request_id(self) -> u64 {
        match self {
            Self::Starting { request_id, .. } | Self::Stopping { request_id, .. } => request_id,
        }
    }
}

struct WebUiFailureState {
    surface: Option<MobileWebUiSurface>,
    failure: MobileWebUiFailure,
}

#[derive(Default)]
struct WebUiLifecycle {
    revision: u64,
    next_request_id: u64,
    desired: Option<WebUiIntent>,
    active: Option<ActiveWebUi>,
    transition: Option<WebUiTransition>,
    failure: Option<WebUiFailureState>,
}

impl WebUiLifecycle {
    fn next_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.next_request_id
    }

    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn refresh_server_exit(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if !active.task.is_finished() {
            return;
        }

        let active = self.active.take().expect("active Web UI was just observed");
        if self.next_request_id > active.request_id {
            self.changed();
            return;
        }
        let message = active
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| "embedded Web UI server exited unexpectedly".to_string());
        self.desired = None;
        self.transition = None;
        self.failure = Some(WebUiFailureState {
            surface: Some(active.session.surface),
            failure: MobileWebUiFailure {
                stage: MobileWebUiFailureStage::Server,
                message,
                recovery: MobileWebUiRecovery::Abort,
            },
        });
        self.changed();
    }

    fn snapshot(&mut self) -> MobileWebUiState {
        self.refresh_server_exit();
        let (phase, surface) = if let Some(failure) = self.failure.as_ref() {
            (MobileWebUiPhase::Failed, failure.surface)
        } else if let Some(transition) = self.transition {
            match transition {
                WebUiTransition::Starting { surface, .. } => {
                    (MobileWebUiPhase::Starting, Some(surface))
                }
                WebUiTransition::Stopping { surface, .. } => (MobileWebUiPhase::Stopping, surface),
            }
        } else if let Some(active) = self.active.as_ref() {
            (MobileWebUiPhase::Running, Some(active.session.surface))
        } else {
            (MobileWebUiPhase::Idle, None)
        };

        MobileWebUiState {
            revision: self.revision,
            phase,
            surface,
            session: self.active.as_ref().map(|active| active.session.clone()),
            failure: self.failure.as_ref().map(|failure| failure.failure.clone()),
        }
    }
}

/// Shared, process-wide mobile client and Web UI lifecycle owner.
pub struct MobileClient {
    runtime: Arc<Runtime>,
    options: MobileClientOptions,
    connection: RwLock<Option<ActiveConnection>>,
    connection_build: Mutex<()>,
    title_latency_operation: Mutex<()>,
    title_latency_monitor: Mutex<TitleLatencyMonitor>,
    web_ui_operation: Mutex<()>,
    web_ui: Mutex<WebUiLifecycle>,
}

impl MobileClient {
    pub fn new(options: MobileClientOptions) -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("ironmesh-mobile-client")
            .build()
            .context("failed to create mobile client runtime")?;
        Ok(Self::with_runtime(Arc::new(runtime), options))
    }

    pub fn with_runtime(runtime: Arc<Runtime>, options: MobileClientOptions) -> Self {
        Self {
            runtime,
            options,
            connection: RwLock::new(None),
            connection_build: Mutex::new(()),
            title_latency_operation: Mutex::new(()),
            title_latency_monitor: Mutex::new(TitleLatencyMonitor::disabled()),
            web_ui_operation: Mutex::new(()),
            web_ui: Mutex::new(WebUiLifecycle::default()),
        }
    }

    /// Returns the existing configuration-affine session or atomically replaces
    /// it. Existing leases continue to own their previous client without a lock.
    pub fn connect(&self, configuration: MobileClientConfiguration) -> Result<MobileClientSession> {
        self.connect_prepared(configuration, None)
    }

    /// Resolves repeated FFI input through the active affine session without
    /// reparsing bootstrap and identity JSON for every native operation.
    pub fn connect_input(
        &self,
        connection_input: impl AsRef<str>,
        server_ca_pem: Option<impl AsRef<str>>,
        client_identity_json: Option<impl AsRef<str>>,
    ) -> Result<MobileClientSession> {
        let input = MobileClientInput::new(connection_input, server_ca_pem, client_identity_json);
        {
            let active = self
                .connection
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(active) = active.as_ref()
                && active.source_input.as_ref() == Some(&input)
            {
                return Ok(active.session.clone());
            }
        }

        let configuration = MobileClientConfiguration::from_input(&input)?;
        self.connect_prepared(configuration, Some(input))
    }

    fn connect_prepared(
        &self,
        configuration: MobileClientConfiguration,
        source_input: Option<MobileClientInput>,
    ) -> Result<MobileClientSession> {
        if let Some(session) = self.reuse_connection(configuration.affinity(), &source_input) {
            return Ok(session);
        }

        let _build = self
            .connection_build
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = self.reuse_connection(configuration.affinity(), &source_input) {
            return Ok(session);
        }

        // Session construction may perform bounded discovery and connection
        // probes. Keep it outside the ready-session lock so matching callers
        // can continue cloning the current immutable lease while this runs.
        let session = self.build_session(configuration)?;
        let mut active = self
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = Some(ActiveConnection {
            affinity: session.affinity().clone(),
            source_input,
            session: session.clone(),
        });
        Ok(session)
    }

    fn reuse_connection(
        &self,
        affinity: &MobileConnectionAffinity,
        source_input: &Option<MobileClientInput>,
    ) -> Option<MobileClientSession> {
        let Some(source_input) = source_input else {
            let active = self
                .connection
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return active
                .as_ref()
                .filter(|active| active.affinity == *affinity)
                .map(|active| active.session.clone());
        };

        let mut active = self
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = active
            .as_mut()
            .filter(|active| active.affinity == *affinity)?;
        if active.source_input.as_ref() != Some(source_input) {
            active.source_input = Some(source_input.clone());
        }
        Some(active.session.clone())
    }

    fn build_session(
        &self,
        configuration: MobileClientConfiguration,
    ) -> Result<MobileClientSession> {
        let bootstrap = configuration.effective_bootstrap().clone();
        let (client, managed_client, client_identity) =
            match configuration.client_identity().cloned() {
                Some(identity) => {
                    let original_identity = identity.clone();
                    let managed =
                        self.runtime
                            .block_on(bootstrap.build_managed_client_with_identity(
                                identity,
                                self.options.managed_client.clone(),
                            ))?;
                    let current_identity = managed
                        .latest_identity_update()
                        .unwrap_or(original_identity.clone());
                    if current_identity != original_identity
                        && let Some(persistence) = self.options.identity_persistence.as_ref()
                        && let Err(error) = (persistence.persist)(&current_identity)
                    {
                        tracing::warn!(
                            source = persistence.source,
                            error = %error,
                            "failed to persist renewed mobile client identity"
                        );
                    }
                    (managed.client(), Some(managed), Some(current_identity))
                }
                None => (bootstrap.build_client()?, None, None),
            };

        let client_node = ClientNode::with_client(client.clone());
        Ok(MobileClientSession {
            inner: Arc::new(MobileClientSessionInner {
                id: Uuid::now_v7(),
                runtime: self.runtime.clone(),
                configuration,
                client,
                client_node,
                managed_client,
                client_identity,
            }),
        })
    }

    pub fn web_ui_state(&self) -> MobileWebUiState {
        self.web_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot()
    }

    /// Configures the process-wide latency monitor against a session with the
    /// same affinity rules as every other mobile operation.
    pub fn configure_title_latency_monitor(
        &self,
        configuration: MobileClientConfiguration,
        config: TitleLatencyProbeConfig,
    ) -> Result<TitleLatencyProbeStatus> {
        config.validate()?;
        let _operation = self
            .title_latency_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next_monitor = if config.enabled {
            let session = self.connect(configuration)?;
            TitleLatencyMonitor::start(session.base_client(), config)?
        } else {
            TitleLatencyMonitor::disabled()
        };
        let status = next_monitor.status();
        *self
            .title_latency_monitor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next_monitor;
        Ok(status)
    }

    pub fn title_latency_status(&self) -> TitleLatencyProbeStatus {
        self.title_latency_monitor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .status()
    }

    pub fn stop_title_latency_monitor(&self) -> TitleLatencyProbeStatus {
        let _operation = self
            .title_latency_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let monitor = TitleLatencyMonitor::disabled();
        let status = monitor.status();
        *self
            .title_latency_monitor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = monitor;
        status
    }

    /// Starts or switches the single loopback server. Concurrent requests are
    /// superseded before publishing a session, never run side by side.
    pub fn start_web_ui(
        &self,
        configuration: MobileClientConfiguration,
        surface: MobileWebUiSurface,
    ) -> MobileWebUiCommandResult {
        let affinity = configuration.affinity().clone();
        let request_id = {
            let mut lifecycle = self
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lifecycle.refresh_server_exit();
            let request_id = lifecycle.next_request_id();
            lifecycle.desired = Some(WebUiIntent {
                request_id,
                surface,
                affinity: affinity.clone(),
            });
            lifecycle.transition = Some(WebUiTransition::Starting {
                request_id,
                surface,
            });
            lifecycle.failure = None;
            lifecycle.changed();
            request_id
        };

        let _operation = self
            .web_ui_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let previous_active = {
            let mut lifecycle = self
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lifecycle.refresh_server_exit();
            if !lifecycle.desired.as_ref().is_some_and(|intent| {
                intent.request_id == request_id
                    && intent.surface == surface
                    && intent.affinity == affinity
            }) {
                return MobileWebUiCommandResult {
                    disposition: MobileWebUiCommandDisposition::Superseded,
                    state: lifecycle.snapshot(),
                };
            }
            if lifecycle.active.as_ref().is_some_and(|active| {
                active.affinity == affinity && active.session.surface == surface
            }) {
                lifecycle
                    .active
                    .as_mut()
                    .expect("matching active Web UI was just observed")
                    .request_id = request_id;
                lifecycle.transition = None;
                lifecycle.changed();
                return MobileWebUiCommandResult {
                    disposition: MobileWebUiCommandDisposition::Reused,
                    state: lifecycle.snapshot(),
                };
            }
            let active = lifecycle.active.take();
            if active.is_some() {
                lifecycle.changed();
            }
            active
        };
        if let Some(active) = previous_active {
            self.abort_web_ui_task(active);
        }

        let result = self
            .connect(configuration)
            .and_then(|session| self.build_web_ui(request_id, surface, &session));

        match result {
            Ok(active) => {
                let mut lifecycle = self
                    .web_ui
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !lifecycle.desired.as_ref().is_some_and(|intent| {
                    intent.request_id == request_id
                        && intent.surface == surface
                        && intent.affinity == affinity
                }) {
                    let result = MobileWebUiCommandResult {
                        disposition: MobileWebUiCommandDisposition::Superseded,
                        state: lifecycle.snapshot(),
                    };
                    drop(lifecycle);
                    self.abort_web_ui_task(active);
                    return result;
                }
                lifecycle.active = Some(active);
                lifecycle.transition = None;
                lifecycle.failure = None;
                lifecycle.changed();
                MobileWebUiCommandResult {
                    disposition: MobileWebUiCommandDisposition::Applied,
                    state: lifecycle.snapshot(),
                }
            }
            Err(error) => {
                let mut lifecycle = self
                    .web_ui
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if lifecycle
                    .desired
                    .as_ref()
                    .is_some_and(|intent| intent.request_id == request_id)
                {
                    lifecycle.transition = None;
                    lifecycle.failure = Some(WebUiFailureState {
                        surface: Some(surface),
                        failure: MobileWebUiFailure {
                            stage: MobileWebUiFailureStage::Start,
                            message: format!("{error:#}"),
                            recovery: MobileWebUiRecovery::RetryStartOrAbort,
                        },
                    });
                    lifecycle.changed();
                    MobileWebUiCommandResult {
                        disposition: MobileWebUiCommandDisposition::Applied,
                        state: lifecycle.snapshot(),
                    }
                } else {
                    MobileWebUiCommandResult {
                        disposition: MobileWebUiCommandDisposition::Superseded,
                        state: lifecycle.snapshot(),
                    }
                }
            }
        }
    }

    /// Stops only the requested surface. Closing a stale native presentation
    /// cannot tear down a newer surface that already replaced it.
    pub fn stop_web_ui(&self, surface: MobileWebUiSurface) -> MobileWebUiCommandResult {
        let Some(request_id) = self.begin_web_ui_stop(surface) else {
            return MobileWebUiCommandResult {
                disposition: MobileWebUiCommandDisposition::Noop,
                state: self.web_ui_state(),
            };
        };

        let _operation = self
            .web_ui_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.finish_web_ui_stop(request_id, surface)
    }

    fn begin_web_ui_stop(&self, surface: MobileWebUiSurface) -> Option<u64> {
        let mut lifecycle = self
            .web_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lifecycle.refresh_server_exit();
        let desired_matches = lifecycle
            .desired
            .as_ref()
            .is_some_and(|intent| intent.surface == surface);
        let active_matches = lifecycle
            .active
            .as_ref()
            .is_some_and(|active| active.session.surface == surface);
        let failure_matches = lifecycle
            .failure
            .as_ref()
            .is_some_and(|failure| failure.surface == Some(surface));
        if !desired_matches && !active_matches && !failure_matches {
            return None;
        }

        let request_id = lifecycle.next_request_id();
        if desired_matches {
            lifecycle.desired = None;
        }
        lifecycle.transition = Some(WebUiTransition::Stopping {
            request_id,
            surface: Some(surface),
        });
        lifecycle.failure = None;
        lifecycle.changed();
        Some(request_id)
    }

    fn finish_web_ui_stop(
        &self,
        request_id: u64,
        surface: MobileWebUiSurface,
    ) -> MobileWebUiCommandResult {
        let active = {
            let mut lifecycle = self
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lifecycle.refresh_server_exit();
            // The operation mutex is not FIFO: a newer command may complete
            // before this waiter acquires it, and the latest request must win.
            if lifecycle.next_request_id != request_id {
                return MobileWebUiCommandResult {
                    disposition: MobileWebUiCommandDisposition::Superseded,
                    state: lifecycle.snapshot(),
                };
            }
            let active = lifecycle
                .active
                .as_ref()
                .is_some_and(|active| active.session.surface == surface)
                .then(|| lifecycle.active.take())
                .flatten();
            if lifecycle
                .transition
                .is_some_and(|transition| transition.request_id() == request_id)
            {
                lifecycle.transition = None;
            }
            lifecycle.changed();
            active
        };
        if let Some(active) = active {
            self.abort_web_ui_task(active);
        }
        let mut lifecycle = self
            .web_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        MobileWebUiCommandResult {
            disposition: MobileWebUiCommandDisposition::Applied,
            state: lifecycle.snapshot(),
        }
    }

    /// Infallible recovery boundary used by cache clearing and process
    /// lifecycle teardown. It cancels pending starts and force-aborts any task.
    pub fn abort_web_ui(&self) -> MobileWebUiCommandResult {
        let request_id = {
            let mut lifecycle = self
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let request_id = lifecycle.next_request_id();
            lifecycle.desired = None;
            lifecycle.transition = Some(WebUiTransition::Stopping {
                request_id,
                surface: None,
            });
            lifecycle.failure = None;
            lifecycle.changed();
            request_id
        };

        let _operation = self
            .web_ui_operation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = {
            let mut lifecycle = self
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let active = lifecycle.active.take();
            if lifecycle
                .transition
                .is_some_and(|transition| transition.request_id() == request_id)
            {
                lifecycle.transition = None;
            }
            lifecycle.failure = None;
            lifecycle.changed();
            active
        };
        if let Some(active) = active {
            self.abort_web_ui_task(active);
        }
        let mut lifecycle = self
            .web_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        MobileWebUiCommandResult {
            disposition: MobileWebUiCommandDisposition::Applied,
            state: lifecycle.snapshot(),
        }
    }

    fn build_web_ui(
        &self,
        request_id: u64,
        surface: MobileWebUiSurface,
        client_session: &MobileClientSession,
    ) -> Result<ActiveWebUi> {
        let listener = self
            .runtime
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
            .context("failed to bind embedded Web UI listener")?;
        let address = listener
            .local_addr()
            .context("failed to inspect embedded Web UI listener address")?;
        let authorization = EmbeddedWebUiSessionAuthorization::new();
        let session = MobileWebUiSession {
            session_id: Uuid::now_v7(),
            surface,
            url: format!("http://127.0.0.1:{}/", address.port()),
            authorization: authorization.token().to_string(),
        };

        let mut web_ui_config = WebUiConfig::from_client(
            client_session.client(self.options.web_ui_connection_name.clone()),
        )
        .with_service_name(self.options.web_ui_service_name.clone())
        .with_connection_bootstrap(client_session.configuration().effective_bootstrap().clone())
        .with_embedded_session_authorization(authorization);
        if let Some(identity) = client_session.client_identity() {
            web_ui_config = web_ui_config.with_client_identity(identity);
        }
        if let Some(persistence) = self.options.web_ui_bootstrap_persistence.clone() {
            web_ui_config = web_ui_config.with_connection_bootstrap_persistence(persistence);
        }
        if let Some(log_buffer) = self.options.web_ui_log_buffer.clone() {
            web_ui_config = web_ui_config.with_log_buffer(log_buffer);
        }
        let app = web_ui_backend::router(web_ui_config);
        let completion = Arc::new(Mutex::new(None));
        let task_completion = completion.clone();
        let task = self.runtime.spawn(async move {
            let result = axum::serve(listener, app).await;
            let message = match result {
                Ok(()) => "embedded Web UI server exited unexpectedly".to_string(),
                Err(error) => format!("embedded Web UI server failed: {error}"),
            };
            *task_completion
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
        });

        Ok(ActiveWebUi {
            request_id,
            affinity: client_session.affinity().clone(),
            session,
            task,
            completion,
        })
    }

    fn abort_web_ui_task(&self, active: ActiveWebUi) {
        active.task.abort();
        let _ = self.runtime.block_on(active.task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    fn test_bootstrap(port: u16) -> String {
        format!(
            r#"{{
                "version": 1,
                "cluster_id": "019d04a8-3099-75bc-8ff5-f5bd9a78bb83",
                "rendezvous_urls": [],
                "direct_endpoints": [{{
                    "url": "http://127.0.0.1:{port}",
                    "usage": "public_api",
                    "node_id": "019d04a8-3099-75bc-8ff5-f5bd9a78bb84"
                }}],
                "relay_mode": "disabled",
                "trust_roots": {{}}
            }}"#
        )
    }

    fn invalid_bootstrap() -> String {
        r#"{
            "version": 1,
            "cluster_id": "019d04a8-3099-75bc-8ff5-f5bd9a78bb83",
            "rendezvous_urls": [],
            "direct_endpoints": [],
            "relay_mode": "disabled",
            "trust_roots": {}
        }"#
        .to_string()
    }

    fn configuration(port: u16) -> MobileClientConfiguration {
        MobileClientConfiguration::new(test_bootstrap(port), None::<&str>, None::<&str>)
            .expect("test configuration should parse")
    }

    fn client() -> Arc<MobileClient> {
        Arc::new(
            MobileClient::new(MobileClientOptions::new("test"))
                .expect("test mobile client should start"),
        )
    }

    #[test]
    fn matching_configuration_reuses_one_affine_session() {
        let client = client();
        let first = client
            .connect(configuration(18_080))
            .expect("first session should connect");
        let second = client
            .connect(configuration(18_080))
            .expect("matching session should connect");

        assert_eq!(first.id(), second.id());
        assert_eq!(first.affinity(), second.affinity());
    }

    #[test]
    fn repeated_ffi_input_reuses_one_affine_session() {
        let client = client();
        let first = client
            .connect_input(test_bootstrap(18_080), None::<&str>, None::<&str>)
            .expect("first input should connect");
        let second = client
            .connect_input(test_bootstrap(18_080), None::<&str>, None::<&str>)
            .expect("matching input should connect");

        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn ready_connection_bypasses_session_build_lock() {
        let client = client();
        let ready = client
            .connect(configuration(18_080))
            .expect("initial session should connect");
        let build_guard = client
            .connection_build
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (completed, observed) = mpsc::channel();
        let caller = {
            let client = client.clone();
            thread::spawn(move || {
                let result = client
                    .connect(configuration(18_080))
                    .map(|session| session.id());
                completed
                    .send(result)
                    .expect("test receiver should remain available");
            })
        };

        let observed = observed.recv_timeout(Duration::from_secs(5));
        drop(build_guard);
        caller
            .join()
            .expect("ready-session caller should not panic");

        assert_eq!(
            observed
                .expect("ready session should not wait for an unrelated build")
                .expect("ready session should connect"),
            ready.id()
        );
    }

    #[test]
    fn replacing_configuration_does_not_invalidate_existing_lease() {
        let client = client();
        let first = client
            .connect(configuration(18_080))
            .expect("first session should connect");
        let replacement = client
            .connect(configuration(18_081))
            .expect("replacement session should connect");

        assert_ne!(first.id(), replacement.id());
        assert_ne!(first.affinity(), replacement.affinity());
        assert_eq!(
            first
                .client("old lease")
                .connection_route_snapshot()
                .endpoints[0]
                .locator,
            "http://127.0.0.1:18080"
        );
    }

    #[test]
    fn identity_renewal_metadata_does_not_change_affinity() {
        let cluster_id = "019d04a8-3099-75bc-8ff5-f5bd9a78bb83"
            .parse()
            .expect("cluster id should parse");
        let identity =
            ClientIdentityMaterial::generate(cluster_id, None, Some("Mobile device".to_string()))
                .expect("identity should generate");
        let mut renewed = identity.clone();
        renewed.label = Some("Renamed device".to_string());
        renewed.issued_at_unix = Some(10);
        renewed.expires_at_unix = Some(20);

        let original = MobileClientConfiguration::new(
            test_bootstrap(18_080),
            None::<&str>,
            Some(
                identity
                    .to_json_pretty()
                    .expect("identity should serialize"),
            ),
        )
        .expect("original configuration should parse");
        let renewed = MobileClientConfiguration::new(
            test_bootstrap(18_080),
            None::<&str>,
            Some(renewed.to_json_pretty().expect("identity should serialize")),
        )
        .expect("renewed configuration should parse");

        assert_eq!(original.affinity(), renewed.affinity());
    }

    #[test]
    fn title_latency_monitor_lifecycle_is_owned_by_mobile_client() {
        let client = client();
        let configured = client
            .configure_title_latency_monitor(
                configuration(18_080),
                TitleLatencyProbeConfig {
                    enabled: false,
                    period_seconds: 60,
                },
            )
            .expect("disabled monitor configuration should succeed");

        assert_eq!(configured, client.title_latency_status());
        assert_eq!(configured, client.stop_title_latency_monitor());
    }

    #[test]
    fn mobile_client_options_default_to_interactive_route_maintenance() {
        let options = MobileClientOptions::new("test");
        let expected = ManagedClientOptions::default();

        assert_eq!(options.managed_client.discovery_ttl, expected.discovery_ttl);
        assert_eq!(
            options.managed_client.route_retirement_grace,
            expected.route_retirement_grace
        );
    }

    #[test]
    fn web_ui_switch_has_one_active_loopback_session() {
        let client = client();
        let first = client.start_web_ui(configuration(18_080), MobileWebUiSurface::WebUi);
        let first_session = first.state.session.expect("first Web UI should be running");

        let switched = client.start_web_ui(configuration(18_080), MobileWebUiSurface::GalleryMap);
        let switched_session = switched
            .state
            .session
            .expect("switched Web UI should be running");

        assert_eq!(switched.state.phase, MobileWebUiPhase::Running);
        assert_eq!(switched_session.surface, MobileWebUiSurface::GalleryMap);
        assert_ne!(first_session.session_id, switched_session.session_id);
        assert_eq!(
            client
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .active
                .iter()
                .count(),
            1
        );
    }

    #[test]
    fn stale_surface_stop_does_not_close_replacement() {
        let client = client();
        let _ = client.start_web_ui(configuration(18_080), MobileWebUiSurface::WebUi);
        let switched = client.start_web_ui(configuration(18_080), MobileWebUiSurface::GalleryMap);
        let active_session_id = switched
            .state
            .session
            .as_ref()
            .expect("gallery map should be running")
            .session_id;

        let stale_stop = client.stop_web_ui(MobileWebUiSurface::WebUi);

        assert_eq!(stale_stop.disposition, MobileWebUiCommandDisposition::Noop);
        assert_eq!(stale_stop.state.phase, MobileWebUiPhase::Running);
        assert_eq!(
            stale_stop.state.session.map(|session| session.session_id),
            Some(active_session_id)
        );
    }

    #[test]
    fn delayed_stop_does_not_close_newer_start() {
        let client = client();
        let initial = client.start_web_ui(configuration(18_080), MobileWebUiSurface::WebUi);
        let initial_session_id = initial
            .state
            .session
            .expect("initial Web UI should be running")
            .session_id;
        let delayed_stop_request = client
            .begin_web_ui_stop(MobileWebUiSurface::WebUi)
            .expect("running Web UI should begin stopping");

        let restarted = client.start_web_ui(configuration(18_081), MobileWebUiSurface::WebUi);
        let restarted_session_id = restarted
            .state
            .session
            .as_ref()
            .expect("newer Web UI should be running")
            .session_id;
        let delayed_stop =
            client.finish_web_ui_stop(delayed_stop_request, MobileWebUiSurface::WebUi);

        assert_eq!(
            restarted.disposition,
            MobileWebUiCommandDisposition::Applied
        );
        assert_ne!(restarted_session_id, initial_session_id);
        assert_eq!(
            delayed_stop.disposition,
            MobileWebUiCommandDisposition::Superseded
        );
        assert_eq!(delayed_stop.state.phase, MobileWebUiPhase::Running);
        assert_eq!(
            delayed_stop.state.session.map(|session| session.session_id),
            Some(restarted_session_id)
        );
    }

    #[test]
    fn superseded_start_result_does_not_expose_active_session() {
        let client = client();
        let running = client.start_web_ui(configuration(18_080), MobileWebUiSurface::WebUi);
        let active_session_id = running
            .state
            .session
            .as_ref()
            .expect("Web UI should be running")
            .session_id;
        let superseded = MobileWebUiCommandResult {
            disposition: MobileWebUiCommandDisposition::Superseded,
            state: running.state,
        };

        let error = superseded
            .into_started_session(MobileWebUiSurface::WebUi)
            .expect_err("superseded start must not expose the active session");

        assert!(error.to_string().contains("superseded or cancelled"));
        assert_eq!(
            client
                .web_ui_state()
                .session
                .map(|session| session.session_id),
            Some(active_session_id)
        );
    }

    #[test]
    fn server_exit_does_not_clear_newer_start_intent() {
        let client = client();
        let _ = client.start_web_ui(configuration(18_080), MobileWebUiSurface::WebUi);
        let replacement_configuration = configuration(18_081);
        let finished_task = client.runtime.spawn(async {});
        while !finished_task.is_finished() {
            thread::yield_now();
        }

        let (old_task, replacement_request_id, observed) = {
            let mut lifecycle = client
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let active = lifecycle
                .active
                .as_mut()
                .expect("initial Web UI should be active");
            let old_task = std::mem::replace(&mut active.task, finished_task);
            *active
                .completion
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some("old Web UI exited".to_string());

            let replacement_request_id = lifecycle.next_request_id();
            lifecycle.desired = Some(WebUiIntent {
                request_id: replacement_request_id,
                surface: MobileWebUiSurface::GalleryMap,
                affinity: replacement_configuration.affinity().clone(),
            });
            lifecycle.transition = Some(WebUiTransition::Starting {
                request_id: replacement_request_id,
                surface: MobileWebUiSurface::GalleryMap,
            });
            lifecycle.changed();
            let observed = lifecycle.snapshot();
            (old_task, replacement_request_id, observed)
        };
        old_task.abort();
        let _ = client.runtime.block_on(old_task);

        assert_eq!(observed.phase, MobileWebUiPhase::Starting);
        assert_eq!(observed.surface, Some(MobileWebUiSurface::GalleryMap));
        assert!(observed.session.is_none());
        assert!(observed.failure.is_none());
        let lifecycle = client
            .web_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            lifecycle.desired.as_ref().map(|intent| intent.request_id),
            Some(replacement_request_id)
        );
        assert!(
            lifecycle
                .transition
                .is_some_and(|transition| transition.request_id() == replacement_request_id)
        );
    }

    #[test]
    fn failed_start_is_durable_until_explicit_recovery() {
        let client = client();
        let invalid =
            MobileClientConfiguration::new(invalid_bootstrap(), None::<&str>, None::<&str>)
                .expect("structurally valid bootstrap should parse");

        let failed = client.start_web_ui(invalid, MobileWebUiSurface::WebUi);
        let observed = client.web_ui_state();

        assert_eq!(failed.state.phase, MobileWebUiPhase::Failed);
        assert_eq!(observed, failed.state);
        assert_eq!(
            observed.failure.as_ref().map(|failure| failure.recovery),
            Some(MobileWebUiRecovery::RetryStartOrAbort)
        );

        let recovered = client.abort_web_ui();
        assert_eq!(recovered.state.phase, MobileWebUiPhase::Idle);
        assert!(recovered.state.failure.is_none());
    }

    #[test]
    fn concurrent_starts_publish_only_the_latest_request() {
        let client = client();
        let start = Arc::new(Barrier::new(3));
        let callers = [MobileWebUiSurface::WebUi, MobileWebUiSurface::GalleryMap]
            .into_iter()
            .map(|surface| {
                let client = client.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    client.start_web_ui(configuration(18_080), surface)
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        let results = callers
            .into_iter()
            .map(|caller| caller.join().expect("start caller should not panic"))
            .collect::<Vec<_>>();
        let final_state = client.web_ui_state();

        assert_eq!(final_state.phase, MobileWebUiPhase::Running);
        assert_eq!(
            client
                .web_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .active
                .iter()
                .count(),
            1
        );
        assert!(results.iter().any(|result| {
            matches!(
                result.disposition,
                MobileWebUiCommandDisposition::Applied | MobileWebUiCommandDisposition::Superseded
            )
        }));
    }
}
