use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DESKTOP_CLIENT_CONFIG_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DESKTOP_CLIENT_CONFIG_REVISION: &str = git_version::git_version!(
    fallback = "unknown",
    args = ["--tags", "--always", "--dirty=-dirty", "--abbrev=12"]
);
pub const CONFIG_APP_EXE: &str = if cfg!(windows) {
    "berrykeep-config-app.exe"
} else {
    "berrykeep-config-app"
};
pub const BACKGROUND_LAUNCHER_EXE: &str = if cfg!(windows) {
    "berrykeep-background-launcher.exe"
} else {
    "berrykeep-background-launcher"
};
pub const OS_INTEGRATION_EXE: &str = if cfg!(windows) {
    "berrykeep-os-integration.exe"
} else {
    "berrykeep-os-integration"
};
pub const FOLDER_AGENT_EXE: &str = if cfg!(windows) {
    "berrykeep-folder-agent.exe"
} else {
    "berrykeep-folder-agent"
};
pub const CLIENT_CLI_EXE: &str = if cfg!(windows) {
    "berrykeep.exe"
} else {
    "berrykeep"
};
pub const STARTUP_TASK_ID: &str = "BerryKeepBackgroundLauncher";
pub const PLATFORM_KIND: &str = env::consts::OS;
pub const STARTUP_INTEGRATION_LABEL: &str = if cfg!(windows) {
    "Startup Task"
} else {
    "Autostart"
};
pub const STARTUP_INTEGRATION_VALUE: &str = if cfg!(windows) {
    STARTUP_TASK_ID
} else {
    "berrykeep-config-app-background.desktop"
};
pub const STARTUP_INTEGRATION_NOTE: &str = if cfg!(windows) {
    "Enabled services can restart after sign-in through the packaged startup task."
} else {
    "Packaged Linux installs can start the background config app after graphical sign-in through XDG Autostart. Run berrykeep-config-app --background to start it in the current session."
};
pub const OS_INTEGRATION_MANAGEMENT_SUPPORTED: bool = cfg!(any(windows, target_os = "linux"));

const LOCAL_STATE_ROOT_DIR: &str = "BerryKeep";
const CONFIG_SUBDIR: &str = "desktop-client-config";
const INSTANCE_STORE_FILE_NAME: &str = "instances.json";
const LAST_LAUNCH_REPORT_FILE_NAME: &str = "last-launch-report.json";
const DESKTOP_STATUS_FILE_NAME: &str = "desktop-status.json";
const SERVICE_LOG_SUBDIR: &str = "service-logs";
const SERVICE_DESKTOP_STATUS_SUBDIR: &str = "service-status";
const MANAGED_INSTANCE_STORE_VERSION: u32 = 1;
const LAUNCH_REPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedInstanceStore {
    #[serde(default = "managed_instance_store_version")]
    pub version: u32,
    #[serde(default)]
    pub client_identities: Vec<ClientIdentityConfig>,
    #[serde(default)]
    pub client_cli_instances: Vec<ClientCliInstance>,
    #[serde(default)]
    pub os_integration_instances: Vec<OsIntegrationInstance>,
    #[serde(default)]
    pub folder_agent_instances: Vec<FolderAgentInstance>,
}

impl Default for ManagedInstanceStore {
    fn default() -> Self {
        Self {
            version: MANAGED_INSTANCE_STORE_VERSION,
            client_identities: Vec::new(),
            client_cli_instances: Vec::new(),
            os_integration_instances: Vec::new(),
            folder_agent_instances: Vec::new(),
        }
    }
}

impl ManagedInstanceStore {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading managed instance store {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }

        let store: ManagedInstanceStore = serde_json::from_str(&raw)
            .with_context(|| format!("failed parsing managed instance store {}", path.display()))?;
        store.validate_version(path)?;
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        ensure_parent_dir(path)?;
        let mut store = self.clone();
        store.version = MANAGED_INSTANCE_STORE_VERSION;
        let payload = serde_json::to_vec_pretty(&store)
            .context("failed serializing managed instance store")?;
        fs::write(path, payload)
            .with_context(|| format!("failed writing managed instance store {}", path.display()))
    }

    fn validate_version(&self, path: &Path) -> Result<()> {
        if self.version != MANAGED_INSTANCE_STORE_VERSION {
            bail!(
                "unsupported managed instance store version {} in {} (current={})",
                self.version,
                path.display(),
                MANAGED_INSTANCE_STORE_VERSION
            );
        }
        Ok(())
    }

    pub fn upsert_os_integration(&mut self, instance: OsIntegrationInstance) {
        if let Some(existing) = self
            .os_integration_instances
            .iter_mut()
            .find(|candidate| candidate.id == instance.id)
        {
            *existing = instance;
        } else {
            self.os_integration_instances.push(instance);
        }
        self.sort();
    }

    pub fn upsert_client_identity(&mut self, identity: ClientIdentityConfig) {
        if let Some(existing) = self
            .client_identities
            .iter_mut()
            .find(|candidate| candidate.id == identity.id)
        {
            *existing = identity;
        } else {
            self.client_identities.push(identity);
        }
        self.sort();
    }

    pub fn upsert_client_cli(&mut self, instance: ClientCliInstance) {
        if let Some(existing) = self
            .client_cli_instances
            .iter_mut()
            .find(|candidate| candidate.id == instance.id)
        {
            *existing = instance;
        } else {
            self.client_cli_instances.push(instance);
        }
        self.sort();
    }

    pub fn upsert_folder_agent(&mut self, instance: FolderAgentInstance) {
        if let Some(existing) = self
            .folder_agent_instances
            .iter_mut()
            .find(|candidate| candidate.id == instance.id)
        {
            *existing = instance;
        } else {
            self.folder_agent_instances.push(instance);
        }
        self.sort();
    }

    pub fn client_identity(&self, id: &str) -> Option<&ClientIdentityConfig> {
        self.client_identities
            .iter()
            .find(|candidate| candidate.id == id)
    }

    pub fn remove_client_identity(&mut self, id: &str) -> bool {
        let initial_len = self.client_identities.len();
        self.client_identities
            .retain(|candidate| candidate.id != id);
        initial_len != self.client_identities.len()
    }

    pub fn remove_os_integration(&mut self, id: &str) -> bool {
        let initial_len = self.os_integration_instances.len();
        self.os_integration_instances
            .retain(|candidate| candidate.id != id);
        initial_len != self.os_integration_instances.len()
    }

    pub fn remove_client_cli(&mut self, id: &str) -> bool {
        let initial_len = self.client_cli_instances.len();
        self.client_cli_instances
            .retain(|candidate| candidate.id != id);
        initial_len != self.client_cli_instances.len()
    }

    pub fn remove_folder_agent(&mut self, id: &str) -> bool {
        let initial_len = self.folder_agent_instances.len();
        self.folder_agent_instances
            .retain(|candidate| candidate.id != id);
        initial_len != self.folder_agent_instances.len()
    }

    fn sort(&mut self) {
        self.client_identities.sort_by(|left, right| {
            left.label
                .to_ascii_lowercase()
                .cmp(&right.label.to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        self.client_cli_instances.sort_by(|left, right| {
            left.label
                .to_ascii_lowercase()
                .cmp(&right.label.to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        self.os_integration_instances.sort_by(|left, right| {
            left.label
                .to_ascii_lowercase()
                .cmp(&right.label.to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        self.folder_agent_instances.sort_by(|left, right| {
            left.label
                .to_ascii_lowercase()
                .cmp(&right.label.to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentityConfig {
    pub id: String,
    pub label: String,
    pub bootstrap_file: String,
    pub client_identity_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ca_pem_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous_client_identity_expires_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_enrolled_at_unix_ms: Option<u64>,
}

impl ClientIdentityConfig {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("client identity id must not be empty");
        }
        if self.label.trim().is_empty() {
            bail!("client identity label must not be empty");
        }
        if self.bootstrap_file.trim().is_empty() {
            bail!("client identity bootstrap_file must not be empty");
        }
        if self.client_identity_file.trim().is_empty() {
            bail!("client identity client_identity_file must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsIntegrationInstance {
    pub id: String,
    pub label: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_root_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub root_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_file: Option<String>,
    #[serde(
        default,
        alias = "server_ca_cert",
        alias = "server_ca_pem_file",
        skip_serializing_if = "Option::is_none"
    )]
    pub server_ca_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_edge_state_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_name: Option<String>,
    #[serde(default)]
    pub allow_other: bool,
    #[serde(default)]
    pub publish_gnome_status: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gnome_status_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_refresh_interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_status_poll_interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
}

impl OsIntegrationInstance {
    pub fn validate(&self) -> Result<()> {
        if !OS_INTEGRATION_MANAGEMENT_SUPPORTED {
            bail!(
                "os-integration instances are only managed by this config surface on Windows and Linux"
            );
        }
        if self.id.trim().is_empty() {
            bail!("os-integration instance id must not be empty");
        }
        if self.label.trim().is_empty() {
            bail!("os-integration instance label must not be empty");
        }
        if self.root_path.trim().is_empty() {
            bail!("os-integration root_path must not be empty");
        }

        #[cfg(windows)]
        {
            let sync_root_id = self
                .sync_root_id
                .as_deref()
                .map(str::trim)
                .unwrap_or_default();
            let display_name = self
                .display_name
                .as_deref()
                .map(str::trim)
                .unwrap_or_default();
            if sync_root_id.is_empty() {
                bail!("os-integration sync_root_id must not be empty");
            }
            if display_name.is_empty() {
                bail!("os-integration display_name must not be empty");
            }
        }

        #[cfg(target_os = "linux")]
        {
            let snapshot_file = self
                .snapshot_file
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let server_base_url = self
                .server_base_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let bootstrap_file = self
                .bootstrap_file
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());

            if snapshot_file.is_some() && (server_base_url.is_some() || bootstrap_file.is_some()) {
                bail!(
                    "linux os-integration snapshot_file cannot be combined with server_base_url or bootstrap_file"
                );
            }

            if snapshot_file.is_none() && server_base_url.is_none() && bootstrap_file.is_none() {
                bail!(
                    "linux os-integration requires snapshot_file, server_base_url, or bootstrap_file"
                );
            }

            if self.depth == Some(0) {
                bail!("linux os-integration depth must be greater than zero");
            }
            if self.remote_refresh_interval_ms == Some(0) {
                bail!("linux os-integration remote_refresh_interval_ms must be greater than zero");
            }
            if self.remote_status_poll_interval_ms == Some(0) {
                bail!(
                    "linux os-integration remote_status_poll_interval_ms must be greater than zero"
                );
            }
        }

        Ok(())
    }

    pub fn command_args(&self) -> Vec<String> {
        #[cfg(windows)]
        {
            let mut args = vec![
                "serve".to_string(),
                "--sync-root-id".to_string(),
                self.sync_root_id.clone().unwrap_or_default(),
                "--display-name".to_string(),
                self.display_name.clone().unwrap_or_default(),
                "--root-path".to_string(),
                self.root_path.clone(),
            ];

            push_optional_arg(
                &mut args,
                "--server-base-url",
                self.server_base_url.as_deref(),
            );
            push_optional_arg(&mut args, "--prefix", self.prefix.as_deref());
            push_optional_arg(
                &mut args,
                "--bootstrap-file",
                self.bootstrap_file.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--client-identity-file",
                self.client_identity_file.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--server-ca-cert",
                self.server_ca_path.as_deref(),
            );

            args
        }

        #[cfg(target_os = "linux")]
        {
            let mut args = vec!["--mountpoint".to_string(), self.root_path.clone()];

            push_optional_arg(&mut args, "--snapshot-file", self.snapshot_file.as_deref());
            push_optional_arg(
                &mut args,
                "--server-base-url",
                self.server_base_url.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--bootstrap-file",
                self.bootstrap_file.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--server-ca-pem-file",
                self.server_ca_path.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--client-identity-file",
                self.client_identity_file.as_deref(),
            );
            push_optional_arg(
                &mut args,
                "--client-edge-state-dir",
                self.client_edge_state_dir.as_deref(),
            );
            push_optional_arg(&mut args, "--prefix", self.prefix.as_deref());
            push_optional_arg(&mut args, "--fs-name", self.fs_name.as_deref());
            push_optional_arg(
                &mut args,
                "--gnome-status-file",
                self.gnome_status_file.as_deref(),
            );
            push_optional_u64(
                &mut args,
                "--remote-refresh-interval-ms",
                self.remote_refresh_interval_ms,
            );
            push_optional_u64(
                &mut args,
                "--remote-status-poll-interval-ms",
                self.remote_status_poll_interval_ms,
            );
            push_optional_usize(&mut args, "--depth", self.depth);

            if self.allow_other {
                args.push("--allow-other".to_string());
            }
            if self.publish_gnome_status {
                args.push("--publish-gnome-status".to_string());
            }

            args
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderAgentInstance {
    pub id: String,
    pub label: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub root_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ca_pem_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_bind: Option<String>,
    #[serde(default)]
    pub run_once: bool,
    #[serde(default)]
    pub no_watch_local: bool,
}

impl FolderAgentInstance {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("folder-agent instance id must not be empty");
        }
        if self.label.trim().is_empty() {
            bail!("folder-agent instance label must not be empty");
        }
        if self.root_dir.trim().is_empty() {
            bail!("folder-agent root_dir must not be empty");
        }
        Ok(())
    }

    pub fn command_args(&self) -> Vec<String> {
        let mut args = vec!["--root-dir".to_string(), self.root_dir.clone()];

        push_optional_arg(
            &mut args,
            "--state-root-dir",
            self.state_root_dir.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--server-base-url",
            self.server_base_url.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--bootstrap-file",
            self.bootstrap_file.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--server-ca-pem-file",
            self.server_ca_pem_file.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--client-identity-file",
            self.client_identity_file.as_deref(),
        );
        push_optional_arg(&mut args, "--prefix", self.prefix.as_deref());
        push_optional_arg(&mut args, "--ui-bind", self.ui_bind.as_deref());

        if self.run_once {
            args.push("--run-once".to_string());
        }
        if self.no_watch_local {
            args.push("--no-watch-local".to_string());
        }

        args
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCliInstance {
    pub id: String,
    pub label: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ca_pem_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_identity_file: Option<String>,
}

impl ClientCliInstance {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("client-cli instance id must not be empty");
        }
        if self.label.trim().is_empty() {
            bail!("client-cli instance label must not be empty");
        }
        if let Some(bind) = self
            .bind
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            bind.parse::<SocketAddr>().with_context(|| {
                format!(
                    "client-cli bind address '{}' must be a valid host:port",
                    bind
                )
            })?;
        }
        Ok(())
    }

    pub fn command_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        push_optional_arg(
            &mut args,
            "--server-base-url",
            self.server_base_url.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--bootstrap-file",
            self.bootstrap_file.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--server-ca-pem-file",
            self.server_ca_pem_file.as_deref(),
        );
        push_optional_arg(
            &mut args,
            "--client-identity-file",
            self.client_identity_file.as_deref(),
        );
        args.push("serve-web".to_string());
        push_optional_arg(&mut args, "--bind", self.bind.as_deref());

        args
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchReport {
    #[serde(default = "launch_report_version")]
    pub version: u32,
    pub launched_at_unix_ms: u64,
    pub package_root: String,
    pub total_enabled: usize,
    pub outcomes: Vec<LaunchOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub instance_kind: String,
    pub id: String,
    pub label: String,
    pub executable: String,
    pub command_line: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
    pub pid: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceRuntimeStatus {
    pub instance_kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_launch: Option<LaunchOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StopOutcome {
    pub instance_kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub was_running: bool,
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn default_instance_store_path() -> PathBuf {
    #[cfg(windows)]
    {
        local_appdata_root()
            .join(CONFIG_SUBDIR)
            .join(INSTANCE_STORE_FILE_NAME)
    }

    #[cfg(not(windows))]
    {
        config_home_root()
            .join(CONFIG_SUBDIR)
            .join(INSTANCE_STORE_FILE_NAME)
    }
}

/// Migrates BerryKeep state written by releases that relied on
/// MSIX AppData write virtualization into the shared LocalAppData location.
///
/// The migration only runs from a packaged Windows process and never replaces
/// existing external state. This keeps `%LOCALAPPDATA%\\BerryKeep`
/// available to both the packaged app and an unpackaged CLI installation.
pub fn migrate_legacy_virtualized_windows_state() -> Result<bool> {
    #[cfg(windows)]
    {
        let Some(package_family_name) = current_package_family_name_from_executable()? else {
            return Ok(false);
        };
        let legacy_state_dir = local_appdata_base_dir()
            .join("Packages")
            .join(package_family_name)
            .join("LocalCache")
            .join("Local")
            .join(LOCAL_STATE_ROOT_DIR);
        migrate_legacy_virtualized_state_dir(&legacy_state_dir, &local_appdata_root())
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

pub fn default_launch_report_path() -> PathBuf {
    #[cfg(windows)]
    {
        local_appdata_root()
            .join(CONFIG_SUBDIR)
            .join(LAST_LAUNCH_REPORT_FILE_NAME)
    }

    #[cfg(not(windows))]
    {
        state_home_root()
            .join(CONFIG_SUBDIR)
            .join(LAST_LAUNCH_REPORT_FILE_NAME)
    }
}

pub fn default_service_log_dir() -> PathBuf {
    #[cfg(windows)]
    {
        local_appdata_root()
            .join(CONFIG_SUBDIR)
            .join(SERVICE_LOG_SUBDIR)
    }

    #[cfg(not(windows))]
    {
        state_home_root()
            .join(CONFIG_SUBDIR)
            .join(SERVICE_LOG_SUBDIR)
    }
}

pub fn default_service_log_file_path(instance_kind: &str, id: &str) -> PathBuf {
    service_log_file_path(&default_service_log_dir(), instance_kind, id)
}

pub fn default_desktop_status_file_path() -> PathBuf {
    #[cfg(windows)]
    {
        local_appdata_root()
            .join(CONFIG_SUBDIR)
            .join(DESKTOP_STATUS_FILE_NAME)
    }

    #[cfg(not(windows))]
    {
        state_home_root()
            .join(CONFIG_SUBDIR)
            .join(DESKTOP_STATUS_FILE_NAME)
    }
}

pub fn default_service_desktop_status_dir() -> PathBuf {
    #[cfg(windows)]
    {
        local_appdata_root()
            .join(CONFIG_SUBDIR)
            .join(SERVICE_DESKTOP_STATUS_SUBDIR)
    }

    #[cfg(not(windows))]
    {
        state_home_root()
            .join(CONFIG_SUBDIR)
            .join(SERVICE_DESKTOP_STATUS_SUBDIR)
    }
}

pub fn service_desktop_status_file_path(instance_kind: &str, id: &str) -> PathBuf {
    default_service_desktop_status_dir().join(format!(
        "{}-{}.json",
        sanitize_log_file_component(instance_kind),
        sanitize_log_file_component(id)
    ))
}

pub fn load_last_launch_report(path: &Path) -> Result<Option<LaunchReport>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed reading launch report {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }

    let report: LaunchReport = serde_json::from_str(&raw)
        .with_context(|| format!("failed parsing launch report {}", path.display()))?;
    validate_launch_report_version(&report, path)?;
    Ok(Some(report))
}

pub fn save_launch_report(path: &Path, report: &LaunchReport) -> Result<()> {
    ensure_parent_dir(path)?;
    let mut report = report.clone();
    report.version = LAUNCH_REPORT_VERSION;
    let payload = serde_json::to_vec_pretty(&report).context("failed serializing launch report")?;
    fs::write(path, payload)
        .with_context(|| format!("failed writing launch report {}", path.display()))
}

pub fn package_root_from_current_exe() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed locating current executable")?;
    current_exe
        .parent()
        .map(Path::to_path_buf)
        .context("failed locating package root from current executable path")
}

pub fn generate_instance_id(prefix: &str) -> String {
    format!("{}-{}", prefix, unix_ts_ms())
}

pub fn launch_enabled_instances(store: &ManagedInstanceStore, package_root: &Path) -> LaunchReport {
    let mut outcomes = Vec::new();
    let launched_at_unix_ms = unix_ts_ms();
    let service_log_dir = default_service_log_dir();

    for instance in &store.client_cli_instances {
        if !instance.enabled {
            continue;
        }
        outcomes.push(spawn_instance(
            "client-cli",
            &instance.id,
            &instance.label,
            service_executable_candidates(package_root, CLIENT_CLI_EXE),
            instance.command_args(),
            &service_log_dir,
            launched_at_unix_ms,
        ));
    }

    for instance in &store.os_integration_instances {
        if !instance.enabled {
            continue;
        }
        #[cfg(any(windows, target_os = "linux"))]
        {
            outcomes.push(spawn_instance(
                "os-integration",
                &instance.id,
                &instance.label,
                service_executable_candidates(package_root, OS_INTEGRATION_EXE),
                with_managed_desktop_status_args(
                    instance.command_args(),
                    "os-integration",
                    &instance.id,
                ),
                &service_log_dir,
                launched_at_unix_ms,
            ));
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        {
            outcomes.push(unsupported_instance(
                "os-integration",
                &instance.id,
                &instance.label,
                package_root.join(OS_INTEGRATION_EXE),
                "os-integration instances are currently managed only on Windows",
            ));
        }
    }

    for instance in &store.folder_agent_instances {
        if !instance.enabled {
            continue;
        }
        outcomes.push(spawn_instance(
            "folder-agent",
            &instance.id,
            &instance.label,
            service_executable_candidates(package_root, FOLDER_AGENT_EXE),
            with_managed_desktop_status_args(instance.command_args(), "folder-agent", &instance.id),
            &service_log_dir,
            launched_at_unix_ms,
        ));
    }

    LaunchReport {
        version: LAUNCH_REPORT_VERSION,
        launched_at_unix_ms,
        package_root: package_root.display().to_string(),
        total_enabled: outcomes.len(),
        outcomes,
    }
}

pub fn launch_os_integration_instance(
    instance: &OsIntegrationInstance,
    package_root: &Path,
) -> LaunchOutcome {
    #[cfg(any(windows, target_os = "linux"))]
    let launched_at_unix_ms = unix_ts_ms();
    #[cfg(any(windows, target_os = "linux"))]
    {
        spawn_instance(
            "os-integration",
            &instance.id,
            &instance.label,
            service_executable_candidates(package_root, OS_INTEGRATION_EXE),
            with_managed_desktop_status_args(
                instance.command_args(),
                "os-integration",
                &instance.id,
            ),
            &default_service_log_dir(),
            launched_at_unix_ms,
        )
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        unsupported_instance(
            "os-integration",
            &instance.id,
            &instance.label,
            package_root.join(OS_INTEGRATION_EXE),
            "os-integration instances are currently managed only on Windows",
        )
    }
}

pub fn launch_client_cli_instance(
    instance: &ClientCliInstance,
    package_root: &Path,
) -> LaunchOutcome {
    spawn_instance(
        "client-cli",
        &instance.id,
        &instance.label,
        service_executable_candidates(package_root, CLIENT_CLI_EXE),
        instance.command_args(),
        &default_service_log_dir(),
        unix_ts_ms(),
    )
}

pub fn launch_folder_agent_instance(
    instance: &FolderAgentInstance,
    package_root: &Path,
) -> LaunchOutcome {
    spawn_instance(
        "folder-agent",
        &instance.id,
        &instance.label,
        service_executable_candidates(package_root, FOLDER_AGENT_EXE),
        with_managed_desktop_status_args(instance.command_args(), "folder-agent", &instance.id),
        &default_service_log_dir(),
        unix_ts_ms(),
    )
}

#[cfg(target_os = "linux")]
fn with_managed_desktop_status_args(
    mut args: Vec<String>,
    instance_kind: &str,
    id: &str,
) -> Vec<String> {
    remove_flag(&mut args, "--publish-gnome-status", false);
    remove_flag(&mut args, "--gnome-status-file", true);

    args.push("--publish-gnome-status".to_string());
    args.push("--gnome-status-file".to_string());
    args.push(
        service_desktop_status_file_path(instance_kind, id)
            .display()
            .to_string(),
    );
    args
}

#[cfg(not(target_os = "linux"))]
fn with_managed_desktop_status_args(
    args: Vec<String>,
    _instance_kind: &str,
    _id: &str,
) -> Vec<String> {
    args
}

#[cfg(target_os = "linux")]
fn remove_flag(args: &mut Vec<String>, flag: &str, has_value: bool) {
    let mut index = 0;
    while index < args.len() {
        if args[index] != flag {
            index += 1;
            continue;
        }

        args.remove(index);
        if has_value && index < args.len() {
            args.remove(index);
        }
    }
}

pub fn service_runtime_statuses(
    store: &ManagedInstanceStore,
    report: Option<&LaunchReport>,
) -> Vec<ServiceRuntimeStatus> {
    let mut statuses = Vec::new();

    for instance in &store.client_cli_instances {
        statuses.push(service_runtime_status(
            "client-cli",
            &instance.id,
            report.and_then(|report| last_launch_outcome(report, "client-cli", &instance.id)),
        ));
    }

    for instance in &store.os_integration_instances {
        statuses.push(service_runtime_status(
            "os-integration",
            &instance.id,
            report.and_then(|report| last_launch_outcome(report, "os-integration", &instance.id)),
        ));
    }

    for instance in &store.folder_agent_instances {
        statuses.push(service_runtime_status(
            "folder-agent",
            &instance.id,
            report.and_then(|report| last_launch_outcome(report, "folder-agent", &instance.id)),
        ));
    }

    statuses
}

pub fn launch_report_with_updated_outcome(
    existing: Option<LaunchReport>,
    package_root: &Path,
    outcome: LaunchOutcome,
) -> LaunchReport {
    let mut report = existing.unwrap_or_else(|| LaunchReport {
        version: LAUNCH_REPORT_VERSION,
        launched_at_unix_ms: unix_ts_ms(),
        package_root: package_root.display().to_string(),
        total_enabled: 0,
        outcomes: Vec::new(),
    });
    report.version = LAUNCH_REPORT_VERSION;
    report.launched_at_unix_ms = unix_ts_ms();
    report.package_root = package_root.display().to_string();
    report.outcomes.retain(|candidate| {
        candidate.instance_kind != outcome.instance_kind || candidate.id != outcome.id
    });
    report.outcomes.push(outcome);
    report.total_enabled = report.outcomes.len();
    report
}

pub fn stop_service_from_report(
    report: Option<&LaunchReport>,
    instance_kind: &str,
    id: &str,
) -> StopOutcome {
    let Some(outcome) = report.and_then(|report| last_launch_outcome(report, instance_kind, id))
    else {
        return StopOutcome {
            instance_kind: instance_kind.to_string(),
            id: id.to_string(),
            pid: None,
            was_running: false,
            stopped: false,
            error: None,
        };
    };
    let Some(pid) = outcome.pid else {
        return StopOutcome {
            instance_kind: instance_kind.to_string(),
            id: id.to_string(),
            pid: None,
            was_running: false,
            stopped: false,
            error: None,
        };
    };

    if !process_is_running(pid) {
        return StopOutcome {
            instance_kind: instance_kind.to_string(),
            id: id.to_string(),
            pid: Some(pid),
            was_running: false,
            stopped: false,
            error: None,
        };
    }

    match terminate_process(pid) {
        Ok(()) => {
            let stopped = wait_for_process_exit(pid, Duration::from_secs(2));
            StopOutcome {
                instance_kind: instance_kind.to_string(),
                id: id.to_string(),
                pid: Some(pid),
                was_running: true,
                stopped,
                error: if stopped {
                    None
                } else {
                    Some(format!("process {pid} did not exit within the timeout"))
                },
            }
        }
        Err(error) => StopOutcome {
            instance_kind: instance_kind.to_string(),
            id: id.to_string(),
            pid: Some(pid),
            was_running: true,
            stopped: false,
            error: Some(error.to_string()),
        },
    }
}

fn service_runtime_status(
    instance_kind: &str,
    id: &str,
    last_launch: Option<&LaunchOutcome>,
) -> ServiceRuntimeStatus {
    let running_pid = last_launch
        .and_then(|outcome| outcome.pid)
        .filter(|pid| process_is_running(*pid));
    let log_file = last_launch
        .and_then(|outcome| outcome.log_file.clone())
        .or_else(|| {
            Some(
                default_service_log_file_path(instance_kind, id)
                    .display()
                    .to_string(),
            )
        });
    ServiceRuntimeStatus {
        instance_kind: instance_kind.to_string(),
        id: id.to_string(),
        log_file,
        running: running_pid.is_some(),
        pid: running_pid,
        last_launch: last_launch.cloned(),
    }
}

fn last_launch_outcome<'a>(
    report: &'a LaunchReport,
    instance_kind: &str,
    id: &str,
) -> Option<&'a LaunchOutcome> {
    report
        .outcomes
        .iter()
        .rev()
        .find(|outcome| outcome.instance_kind == instance_kind && outcome.id == id)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let started_at = std::time::Instant::now();
    while started_at.elapsed() < timeout {
        if !process_is_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !process_is_running(pid)
}

fn process_is_running(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    let pids = [sysinfo::Pid::from_u32(pid)];
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
    system.process(pids[0]).is_some()
}

fn terminate_process(pid: u32) -> Result<()> {
    let mut system = sysinfo::System::new();
    let pids = [sysinfo::Pid::from_u32(pid)];
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
    let Some(process) = system.process(pids[0]) else {
        return Ok(());
    };
    if process.kill() {
        Ok(())
    } else {
        bail!("failed terminating process {pid}")
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn unsupported_instance(
    instance_kind: &str,
    id: &str,
    label: &str,
    executable_path: PathBuf,
    message: &str,
) -> LaunchOutcome {
    LaunchOutcome {
        instance_kind: instance_kind.to_string(),
        id: id.to_string(),
        label: label.to_string(),
        executable: executable_path.display().to_string(),
        command_line: Vec::new(),
        log_file: None,
        pid: None,
        error: Some(message.to_string()),
    }
}

fn spawn_instance(
    instance_kind: &str,
    id: &str,
    label: &str,
    executable_candidates: Vec<PathBuf>,
    command_line: Vec<String>,
    service_log_dir: &Path,
    launched_at_unix_ms: u64,
) -> LaunchOutcome {
    let executable_candidates = if executable_candidates.is_empty() {
        vec![PathBuf::from(instance_kind)]
    } else {
        executable_candidates
    };
    let executable_candidate_display = executable_candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let mut executable = executable_candidate_display
        .first()
        .cloned()
        .unwrap_or_else(|| instance_kind.to_string());
    let log_file_path = service_log_file_path(service_log_dir, instance_kind, id);
    let log_file = Some(log_file_path.display().to_string());
    let mut log_setup_error = None;
    let mut log_handle = match open_service_log_file(&log_file_path) {
        Ok(file) => Some(file),
        Err(error) => {
            log_setup_error = Some(format!(
                "failed preparing service log file {}: {error}",
                log_file_path.display()
            ));
            None
        }
    };

    if let Some(file) = log_handle.as_mut()
        && let Err(error) = write_launch_log_header(
            file,
            launched_at_unix_ms,
            instance_kind,
            id,
            label,
            &executable_candidate_display,
            &command_line,
        )
    {
        log_setup_error = Some(format!(
            "failed writing service log header {}: {error}",
            log_file_path.display()
        ));
    }

    let mut spawn_errors = Vec::new();
    for executable_path in &executable_candidates {
        executable = executable_path.display().to_string();
        let (stdout, stderr) = match service_log_stdio_pair(log_handle.as_ref()) {
            Ok(stdio) => stdio,
            Err(error) => {
                log_setup_error = Some(error);
                (Stdio::null(), Stdio::null())
            }
        };

        let mut command = Command::new(executable_path);
        command
            .args(&command_line)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        configure_background_service_command(&mut command);

        if let Some(file) = log_handle.as_mut() {
            let _ = writeln!(file, "spawn attempt executable={}", executable);
            let _ = file.flush();
        }

        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                spawn_background_child_reaper(
                    child,
                    format!("{instance_kind}:{id}"),
                    log_handle.as_ref(),
                );
                if let Some(file) = log_handle.as_mut() {
                    let _ = writeln!(file, "spawned pid={pid} executable={}", executable);
                    let _ = file.flush();
                }
                return LaunchOutcome {
                    instance_kind: instance_kind.to_string(),
                    id: id.to_string(),
                    label: label.to_string(),
                    executable,
                    command_line,
                    log_file,
                    pid: Some(pid),
                    error: log_setup_error,
                };
            }
            Err(error) => {
                if let Some(file) = log_handle.as_mut() {
                    let _ = writeln!(
                        file,
                        "spawn attempt failed executable={} error={error}",
                        executable
                    );
                    let _ = file.flush();
                }
                spawn_errors.push(format!("{}: {error}", executable));
            }
        }
    }

    let spawn_error = if spawn_errors.is_empty() {
        "no executable candidates were available".to_string()
    } else {
        spawn_errors.join("; ")
    };
    let error = match log_setup_error {
        Some(log_setup_error) => format!("{spawn_error}; {log_setup_error}"),
        None => spawn_error,
    };
    LaunchOutcome {
        instance_kind: instance_kind.to_string(),
        id: id.to_string(),
        label: label.to_string(),
        executable,
        command_line,
        log_file,
        pid: None,
        error: Some(error),
    }
}

#[cfg(windows)]
fn configure_background_service_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_background_service_command(_command: &mut Command) {}

fn spawn_background_child_reaper(
    mut child: Child,
    child_label: String,
    log_file: Option<&fs::File>,
) {
    let log_file = log_file.and_then(|file| file.try_clone().ok());
    thread::spawn(move || {
        let wait_result = child.wait();
        if let Some(mut file) = log_file {
            match wait_result {
                Ok(status) => {
                    let _ = writeln!(
                        file,
                        "reaped child pid={} label={} status={status}",
                        child.id(),
                        child_label
                    );
                    let _ = file.flush();
                }
                Err(error) => {
                    let _ = writeln!(
                        file,
                        "failed reaping child pid={} label={} error={error}",
                        child.id(),
                        child_label
                    );
                    let _ = file.flush();
                }
            }
        } else if let Err(error) = wait_result {
            eprintln!(
                "desktop-client-config: failed reaping background child pid={} label={}: {error}",
                child.id(),
                child_label
            );
        }
    });
}

fn service_log_stdio_pair(file: Option<&fs::File>) -> Result<(Stdio, Stdio), String> {
    let Some(file) = file else {
        return Ok((Stdio::null(), Stdio::null()));
    };
    match (file.try_clone(), file.try_clone()) {
        (Ok(stdout), Ok(stderr)) => Ok((Stdio::from(stdout), Stdio::from(stderr))),
        (stdout_result, stderr_result) => {
            let stdout_error = stdout_result.err();
            let stderr_error = stderr_result.err();
            let error = match (stdout_error, stderr_error) {
                (Some(stdout_error), Some(stderr_error)) => {
                    format!("failed cloning service log handles: {stdout_error}; {stderr_error}")
                }
                (Some(error), None) | (None, Some(error)) => {
                    format!("failed cloning service log handle: {error}")
                }
                (None, None) => "failed cloning service log handles".to_string(),
            };
            Err(error)
        }
    }
}

fn service_executable_candidates(package_root: &Path, executable_name: &str) -> Vec<PathBuf> {
    let direct = package_root.join(executable_name);
    #[cfg(windows)]
    {
        if is_windows_apps_package_root(package_root)
            && let Some(alias_path) = windows_app_execution_alias_path(executable_name)
            && alias_path != direct
        {
            return vec![alias_path, direct];
        }
    }
    vec![direct]
}

#[cfg(windows)]
fn is_windows_apps_package_root(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
    })
}

#[cfg(windows)]
fn windows_app_execution_alias_path(executable_name: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            path.join("Microsoft")
                .join("WindowsApps")
                .join(executable_name)
        })
}

fn service_log_file_path(service_log_dir: &Path, instance_kind: &str, id: &str) -> PathBuf {
    service_log_dir.join(format!(
        "{}-{}.log",
        sanitize_log_file_component(instance_kind),
        sanitize_log_file_component(id)
    ))
}

fn sanitize_log_file_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('.').trim_matches('_');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn open_service_log_file(path: &Path) -> Result<fs::File> {
    ensure_parent_dir(path)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed opening service log file {}", path.display()))
}

fn write_launch_log_header(
    file: &mut fs::File,
    launched_at_unix_ms: u64,
    instance_kind: &str,
    id: &str,
    label: &str,
    executable_candidates: &[String],
    command_line: &[String],
) -> Result<()> {
    writeln!(file)?;
    writeln!(file, "=== BerryKeep service launch ===")?;
    writeln!(file, "launched_at_unix_ms={launched_at_unix_ms}")?;
    writeln!(file, "instance_kind={instance_kind}")?;
    writeln!(file, "id={id}")?;
    writeln!(file, "label={label}")?;
    writeln!(file, "executable_candidates={executable_candidates:?}")?;
    writeln!(file, "args={command_line:?}")?;
    file.flush()?;
    Ok(())
}

fn push_optional_arg(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };

    args.push(flag.to_string());
    args.push(value.to_string());
}

#[cfg(target_os = "linux")]
fn push_optional_u64(args: &mut Vec<String>, flag: &str, value: Option<u64>) {
    let Some(value) = value else {
        return;
    };

    args.push(flag.to_string());
    args.push(value.to_string());
}

#[cfg(target_os = "linux")]
fn push_optional_usize(args: &mut Vec<String>, flag: &str, value: Option<usize>) {
    let Some(value) = value else {
        return;
    };

    args.push(flag.to_string());
    args.push(value.to_string());
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .with_context(|| format!("failed creating parent directory {}", parent.display()))
}

#[cfg(windows)]
fn local_appdata_base_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(windows)]
fn local_appdata_root() -> PathBuf {
    local_appdata_base_dir().join(LOCAL_STATE_ROOT_DIR)
}

#[cfg(windows)]
fn current_package_family_name_from_executable() -> Result<Option<String>> {
    let package_root = package_root_from_current_exe()?;
    if !is_windows_apps_package_root(&package_root) {
        return Ok(None);
    }

    let package_full_name = package_root
        .file_name()
        .and_then(|name| name.to_str())
        .context("failed reading MSIX package directory name")?;
    Ok(package_family_name_from_full_name(package_full_name))
}

#[cfg(any(windows, test))]
fn package_family_name_from_full_name(package_full_name: &str) -> Option<String> {
    let mut components = package_full_name.rsplitn(5, '_');
    let publisher_id = components.next()?;
    let _resource_id = components.next()?;
    let architecture = components.next()?;
    let version = components.next()?;
    let name = components.next()?;
    if name.is_empty() || publisher_id.is_empty() || architecture.is_empty() || version.is_empty() {
        return None;
    }

    Some(format!("{name}_{publisher_id}"))
}

#[cfg(any(windows, test))]
fn migrate_legacy_virtualized_state_dir(
    legacy_state_dir: &Path,
    external_state_dir: &Path,
) -> Result<bool> {
    if !legacy_state_dir.is_dir() {
        return Ok(false);
    }
    let Some(_migration_lock) = acquire_migration_lock(external_state_dir)? else {
        return Ok(false);
    };

    if !path_exists(external_state_dir)? {
        return copy_state_item_atomically(legacy_state_dir, external_state_dir);
    }

    merge_missing_state_items(legacy_state_dir, external_state_dir)
}

#[cfg(any(windows, test))]
struct MigrationLock {
    path: PathBuf,
}

#[cfg(any(windows, test))]
impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(any(windows, test))]
fn acquire_migration_lock(external_state_dir: &Path) -> Result<Option<MigrationLock>> {
    let lock_path = migration_lock_path(external_state_dir)?;
    let lock_parent = lock_path
        .parent()
        .context("BerryKeep migration lock has no parent directory")?;
    fs::create_dir_all(lock_parent).with_context(|| {
        format!(
            "failed creating BerryKeep migration lock directory {}",
            lock_parent.display()
        )
    })?;

    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{}", std::process::id()) {
                    let _ = fs::remove_file(&lock_path);
                    return Err(error).with_context(|| {
                        format!(
                            "failed writing BerryKeep migration lock {}",
                            lock_path.display()
                        )
                    });
                }
                return Ok(Some(MigrationLock { path: lock_path }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if migration_lock_owner_is_running(&lock_path)? {
                    return Ok(None);
                }
                if let Err(remove_error) = fs::remove_file(&lock_path)
                    && remove_error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(remove_error).with_context(|| {
                        format!(
                            "failed removing stale BerryKeep migration lock {}",
                            lock_path.display()
                        )
                    });
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed acquiring BerryKeep migration lock {}",
                        lock_path.display()
                    )
                });
            }
        }
    }

    Ok(None)
}

#[cfg(any(windows, test))]
fn migration_lock_path(external_state_dir: &Path) -> Result<PathBuf> {
    let parent = external_state_dir
        .parent()
        .context("shared BerryKeep state directory has no parent")?;
    Ok(parent.join(format!(".{LOCAL_STATE_ROOT_DIR}.migration.lock")))
}

#[cfg(any(windows, test))]
fn migration_lock_owner_is_running(lock_path: &Path) -> Result<bool> {
    let owner_pid = fs::read_to_string(lock_path)
        .with_context(|| {
            format!(
                "failed reading BerryKeep migration lock {}",
                lock_path.display()
            )
        })?
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid BerryKeep migration lock {}", lock_path.display()))?;
    Ok(process_is_running(owner_pid))
}

#[cfg(any(windows, test))]
fn merge_missing_state_items(source: &Path, destination: &Path) -> Result<bool> {
    let destination_type = fs::symlink_metadata(destination).with_context(|| {
        format!(
            "failed inspecting existing shared BerryKeep state {}",
            destination.display()
        )
    })?;
    if !destination_type.is_dir() {
        bail!(
            "cannot merge legacy BerryKeep state into non-directory {}",
            destination.display()
        );
    }

    let mut migrated = false;
    for entry in fs::read_dir(source)
        .with_context(|| format!("failed reading legacy BerryKeep state {}", source.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed enumerating legacy BerryKeep state {}",
                source.display()
            )
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let source_type = state_item_file_type(&source_path)?;

        if !path_exists(&destination_path)? {
            migrated |= copy_state_item_atomically(&source_path, &destination_path)?;
            continue;
        }

        let destination_type = state_item_file_type(&destination_path)?;
        match (source_type.is_dir(), destination_type.is_dir()) {
            (true, true) => migrated |= merge_missing_state_items(&source_path, &destination_path)?,
            (false, false) => {}
            _ => bail!(
                "cannot merge legacy BerryKeep state item {} into incompatible shared item {}",
                source_path.display(),
                destination_path.display()
            ),
        }
    }

    Ok(migrated)
}

#[cfg(any(windows, test))]
fn copy_state_item_atomically(source: &Path, destination: &Path) -> Result<bool> {
    if path_exists(destination)? {
        return Ok(false);
    }

    let temporary_destination = migration_temporary_path(destination)?;
    let copy_result = match state_item_file_type(source)?.is_dir() {
        true => copy_directory_recursively(source, &temporary_destination),
        false => copy_file(source, &temporary_destination),
    };
    if let Err(error) = copy_result {
        remove_state_item(&temporary_destination);
        return Err(error);
    }

    if path_exists(destination)? {
        remove_state_item(&temporary_destination);
        return Ok(false);
    }

    if let Err(error) = fs::rename(&temporary_destination, destination) {
        remove_state_item(&temporary_destination);
        if path_exists(destination)? {
            return Ok(false);
        }
        return Err(error).with_context(|| {
            format!(
                "failed atomically publishing migrated BerryKeep state {}",
                destination.display()
            )
        });
    }

    Ok(true)
}

#[cfg(any(windows, test))]
fn copy_directory_recursively(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| {
        format!(
            "failed creating migrated BerryKeep state directory {}",
            destination.display()
        )
    })?;

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed reading legacy BerryKeep state {}", source.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed enumerating legacy BerryKeep state {}",
                source.display()
            )
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if state_item_file_type(&source_path)?.is_dir() {
            copy_directory_recursively(&source_path, &destination_path)?;
        } else {
            copy_file(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

#[cfg(any(windows, test))]
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("migrated BerryKeep state file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed creating migrated BerryKeep state directory {}",
            parent.display()
        )
    })?;
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed migrating BerryKeep state {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(any(windows, test))]
fn state_item_file_type(path: &Path) -> Result<fs::FileType> {
    let file_type = fs::symlink_metadata(path)
        .with_context(|| format!("failed inspecting BerryKeep state item {}", path.display()))?
        .file_type();
    if !file_type.is_dir() && !file_type.is_file() {
        bail!(
            "cannot safely migrate non-regular BerryKeep state item {}",
            path.display()
        );
    }
    Ok(file_type)
}

#[cfg(any(windows, test))]
fn migration_temporary_path(destination: &Path) -> Result<PathBuf> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("migrated BerryKeep state item has no file name")?;
    Ok(destination.with_file_name(format!(
        ".{file_name}.migration-{}-{}",
        std::process::id(),
        unix_ts_ms()
    )))
}

#[cfg(any(windows, test))]
fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed checking BerryKeep state item {}", path.display())),
    }
}

#[cfg(any(windows, test))]
fn remove_state_item(path: &Path) {
    let _ = fs::remove_dir_all(path);
    let _ = fs::remove_file(path);
}

#[cfg(not(windows))]
fn config_home_root() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", &[".config"])
        .unwrap_or_else(std::env::temp_dir)
        .join(LOCAL_STATE_ROOT_DIR)
}

#[cfg(not(windows))]
fn state_home_root() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", &[".local", "state"])
        .unwrap_or_else(std::env::temp_dir)
        .join(LOCAL_STATE_ROOT_DIR)
}

#[cfg(not(windows))]
fn xdg_dir(env_var: &str, home_suffix: &[&str]) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    let mut path = PathBuf::from(home);
    for segment in home_suffix {
        path.push(segment);
    }
    Some(path)
}

fn unix_ts_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn default_enabled() -> bool {
    true
}

fn managed_instance_store_version() -> u32 {
    MANAGED_INSTANCE_STORE_VERSION
}

fn launch_report_version() -> u32 {
    LAUNCH_REPORT_VERSION
}

fn validate_launch_report_version(report: &LaunchReport, path: &Path) -> Result<()> {
    if report.version != LAUNCH_REPORT_VERSION {
        bail!(
            "unsupported launch report version {} in {} (current={})",
            report.version,
            path.display(),
            LAUNCH_REPORT_VERSION
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::{Command, Stdio};

    fn temp_path(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "desktop-client-config-{prefix}-{}-{}",
            std::process::id(),
            unix_ts_ms()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should create");
        dir.join("state.json")
    }

    #[test]
    fn managed_instance_store_roundtrip_persists_version() {
        let path = temp_path("instance-store-roundtrip");
        let store = ManagedInstanceStore {
            client_cli_instances: vec![ClientCliInstance {
                id: "client-1".to_string(),
                label: "Client UI".to_string(),
                enabled: true,
                bind: Some("127.0.0.1:8081".to_string()),
                server_base_url: Some("https://node.example".to_string()),
                bootstrap_file: Some("/tmp/client.bootstrap.json".to_string()),
                server_ca_pem_file: Some("/tmp/server-ca.pem".to_string()),
                client_identity_id: Some("identity-1".to_string()),
                client_identity_file: Some("/tmp/client-identity.json".to_string()),
            }],
            os_integration_instances: vec![OsIntegrationInstance {
                id: "os-1".to_string(),
                label: "Docs".to_string(),
                enabled: true,
                sync_root_id: None,
                display_name: None,
                root_path: "/tmp/docs".to_string(),
                server_base_url: Some("https://node.example".to_string()),
                prefix: None,
                bootstrap_file: None,
                client_identity_id: None,
                snapshot_file: None,
                client_identity_file: None,
                server_ca_path: None,
                client_edge_state_dir: None,
                fs_name: None,
                allow_other: false,
                publish_gnome_status: false,
                gnome_status_file: None,
                remote_refresh_interval_ms: None,
                remote_status_poll_interval_ms: None,
                depth: None,
            }],
            ..ManagedInstanceStore::default()
        };

        store.save(&path).expect("store should save");
        let loaded = ManagedInstanceStore::load_or_default(&path).expect("store should load");

        assert_eq!(loaded.version, MANAGED_INSTANCE_STORE_VERSION);
        assert_eq!(loaded.client_cli_instances.len(), 1);
        assert_eq!(loaded.os_integration_instances.len(), 1);
        assert!(loaded.folder_agent_instances.is_empty());
        assert_eq!(loaded.client_cli_instances[0].id, "client-1");
        assert_eq!(loaded.client_cli_instances[0].label, "Client UI");
        assert_eq!(loaded.os_integration_instances[0].id, "os-1");
        assert_eq!(loaded.os_integration_instances[0].label, "Docs");

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn managed_instance_store_accepts_legacy_missing_version() {
        let path = temp_path("instance-store-legacy");
        std::fs::write(
            &path,
            r#"{
  "os_integration_instances": [],
  "folder_agent_instances": []
}"#,
        )
        .expect("legacy store should write");

        let loaded =
            ManagedInstanceStore::load_or_default(&path).expect("legacy store should load");

        assert_eq!(loaded.version, MANAGED_INSTANCE_STORE_VERSION);

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn managed_instance_store_rejects_future_version() {
        let path = temp_path("instance-store-future");
        std::fs::write(
            &path,
            r#"{
  "version": 99,
  "os_integration_instances": [],
  "folder_agent_instances": []
}"#,
        )
        .expect("future store should write");

        let err = ManagedInstanceStore::load_or_default(&path)
            .expect_err("future store version should fail");
        assert!(
            err.to_string()
                .contains("unsupported managed instance store version 99")
        );

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn launch_report_roundtrip_persists_version() {
        let path = temp_path("launch-report-roundtrip");
        let report = LaunchReport {
            version: LAUNCH_REPORT_VERSION,
            launched_at_unix_ms: 1,
            package_root: "/opt/berrykeep".to_string(),
            total_enabled: 1,
            outcomes: vec![LaunchOutcome {
                instance_kind: "folder-agent".to_string(),
                id: "folder-1".to_string(),
                label: "Folder".to_string(),
                executable: "/opt/berrykeep/berrykeep-folder-agent".to_string(),
                command_line: vec!["--root-dir".to_string(), "/tmp/folder".to_string()],
                log_file: Some("/tmp/berrykeep/folder-agent-folder-1.log".to_string()),
                pid: Some(42),
                error: None,
            }],
        };

        save_launch_report(&path, &report).expect("launch report should save");
        let loaded = load_last_launch_report(&path)
            .expect("launch report should load")
            .expect("launch report should exist");

        assert_eq!(loaded, report);
        assert_eq!(
            OS_INTEGRATION_EXE,
            if cfg!(windows) {
                "berrykeep-os-integration.exe"
            } else {
                "berrykeep-os-integration"
            }
        );

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn service_runtime_statuses_report_current_process_running() {
        let store = ManagedInstanceStore {
            folder_agent_instances: vec![FolderAgentInstance {
                id: "folder-1".to_string(),
                label: "Folder".to_string(),
                enabled: true,
                root_dir: "/tmp/folder".to_string(),
                state_root_dir: None,
                server_base_url: None,
                bootstrap_file: None,
                server_ca_pem_file: None,
                client_identity_id: None,
                client_identity_file: None,
                prefix: None,
                ui_bind: None,
                run_once: false,
                no_watch_local: false,
            }],
            ..ManagedInstanceStore::default()
        };
        let report = LaunchReport {
            version: LAUNCH_REPORT_VERSION,
            launched_at_unix_ms: 1,
            package_root: "/opt/berrykeep".to_string(),
            total_enabled: 1,
            outcomes: vec![LaunchOutcome {
                instance_kind: "folder-agent".to_string(),
                id: "folder-1".to_string(),
                label: "Folder".to_string(),
                executable: std::env::current_exe()
                    .expect("current exe should resolve")
                    .display()
                    .to_string(),
                command_line: Vec::new(),
                log_file: None,
                pid: Some(std::process::id()),
                error: None,
            }],
        };

        let statuses = service_runtime_statuses(&store, Some(&report));

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].instance_kind, "folder-agent");
        assert_eq!(statuses[0].id, "folder-1");
        assert!(
            statuses[0]
                .log_file
                .as_deref()
                .is_some_and(|path| path.ends_with("folder-agent-folder-1.log"))
        );
        assert!(statuses[0].running);
        assert_eq!(statuses[0].pid, Some(std::process::id()));
        assert!(statuses[0].last_launch.is_some());
    }

    #[test]
    fn launch_report_with_updated_outcome_replaces_existing_service_outcome() {
        let old_outcome = LaunchOutcome {
            instance_kind: "folder-agent".to_string(),
            id: "folder-1".to_string(),
            label: "Old".to_string(),
            executable: "old".to_string(),
            command_line: Vec::new(),
            log_file: None,
            pid: Some(1),
            error: None,
        };
        let existing = LaunchReport {
            version: LAUNCH_REPORT_VERSION,
            launched_at_unix_ms: 1,
            package_root: "/old".to_string(),
            total_enabled: 1,
            outcomes: vec![old_outcome],
        };
        let new_outcome = LaunchOutcome {
            instance_kind: "folder-agent".to_string(),
            id: "folder-1".to_string(),
            label: "New".to_string(),
            executable: "new".to_string(),
            command_line: vec!["--root-dir".to_string(), "/tmp/folder".to_string()],
            log_file: Some("/tmp/log".to_string()),
            pid: Some(2),
            error: None,
        };

        let report = launch_report_with_updated_outcome(
            Some(existing),
            std::path::Path::new("package-root"),
            new_outcome.clone(),
        );

        assert_eq!(report.package_root, "package-root");
        assert_eq!(report.total_enabled, 1);
        assert_eq!(report.outcomes, vec![new_outcome]);
    }

    #[test]
    fn stop_service_from_report_ignores_missing_pid() {
        let outcome = LaunchOutcome {
            instance_kind: "folder-agent".to_string(),
            id: "folder-1".to_string(),
            label: "Folder".to_string(),
            executable: "missing".to_string(),
            command_line: Vec::new(),
            log_file: None,
            pid: None,
            error: Some("failed".to_string()),
        };
        let report = LaunchReport {
            version: LAUNCH_REPORT_VERSION,
            launched_at_unix_ms: 1,
            package_root: "/package".to_string(),
            total_enabled: 1,
            outcomes: vec![outcome],
        };

        let outcome = stop_service_from_report(Some(&report), "folder-agent", "folder-1");

        assert_eq!(outcome.instance_kind, "folder-agent");
        assert_eq!(outcome.id, "folder-1");
        assert_eq!(outcome.pid, None);
        assert!(!outcome.was_running);
        assert!(!outcome.stopped);
        assert_eq!(outcome.error, None);
    }

    #[test]
    fn spawn_instance_records_log_file_and_spawn_failure() {
        let path = temp_path("service-log-spawn-error");
        let dir = path.parent().expect("temp path should have parent");
        let log_dir = dir.join("logs");
        let missing_executable = dir.join(FOLDER_AGENT_EXE);

        let outcome = spawn_instance(
            "folder-agent",
            "folder/one",
            "Folder One",
            vec![missing_executable],
            vec!["--root-dir".to_string(), "/tmp/folder".to_string()],
            &log_dir,
            123,
        );

        assert_eq!(outcome.instance_kind, "folder-agent");
        assert_eq!(outcome.id, "folder/one");
        assert!(outcome.pid.is_none());
        assert!(outcome.error.is_some());
        let log_file = outcome.log_file.expect("log file should be reported");
        assert!(
            log_file.ends_with("folder-agent-folder_one.log"),
            "unexpected log file path: {log_file}"
        );
        let log = std::fs::read_to_string(&log_file).expect("log file should be readable");
        assert!(log.contains("=== BerryKeep service launch ==="));
        assert!(log.contains("instance_kind=folder-agent"));
        assert!(log.contains("id=folder/one"));
        assert!(log.contains("spawn attempt failed executable="));

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[cfg(windows)]
    #[test]
    fn windows_apps_package_root_prefers_app_execution_alias() {
        let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let package_root = PathBuf::from(
            r"C:\Program Files\WindowsApps\UlrichHornung.BerryKeep_1.0.2.1_neutral__bnh81bg69mtt8",
        );

        let candidates = service_executable_candidates(&package_root, OS_INTEGRATION_EXE);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from(local_appdata)
                    .join("Microsoft")
                    .join("WindowsApps")
                    .join(OS_INTEGRATION_EXE),
                package_root.join(OS_INTEGRATION_EXE),
            ]
        );
    }

    #[test]
    fn launch_report_accepts_legacy_missing_version() {
        let path = temp_path("launch-report-legacy");
        std::fs::write(
            &path,
            r#"{
  "launched_at_unix_ms": 1,
  "package_root": "/opt/berrykeep",
  "total_enabled": 0,
  "outcomes": []
}"#,
        )
        .expect("legacy launch report should write");

        let loaded = load_last_launch_report(&path)
            .expect("legacy launch report should load")
            .expect("launch report should exist");

        assert_eq!(loaded.version, LAUNCH_REPORT_VERSION);

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn launch_report_rejects_future_version() {
        let path = temp_path("launch-report-future");
        std::fs::write(
            &path,
            r#"{
  "version": 99,
  "launched_at_unix_ms": 1,
  "package_root": "/opt/berrykeep",
  "total_enabled": 0,
  "outcomes": []
}"#,
        )
        .expect("future launch report should write");

        let err =
            load_last_launch_report(&path).expect_err("future launch report version should fail");
        assert!(
            err.to_string()
                .contains("unsupported launch report version 99")
        );

        let _ = std::fs::remove_file(&path);
        let _ = path.parent().map(std::fs::remove_dir_all);
    }

    #[test]
    fn legacy_virtualized_state_migration_merges_missing_entries_without_overwriting_shared_state()
    {
        let path = temp_path("legacy-virtualized-state-migration");
        let root = path.parent().expect("temp path should have parent");
        let legacy_state_dir = root.join("legacy").join(LOCAL_STATE_ROOT_DIR);
        let external_state_dir = root.join("external").join(LOCAL_STATE_ROOT_DIR);
        let legacy_config_dir = legacy_state_dir.join(CONFIG_SUBDIR);
        let external_config_dir = external_state_dir.join(CONFIG_SUBDIR);
        let legacy_identity = legacy_config_dir
            .join("client-identities")
            .join("identity.json");
        let legacy_sync_root_identity = legacy_state_dir
            .join("sync-roots")
            .join("atuh-main")
            .join("client-identity.json");
        std::fs::create_dir_all(
            legacy_identity
                .parent()
                .expect("legacy identity should have a parent"),
        )
        .expect("legacy identity parent should create");
        std::fs::write(&legacy_identity, "legacy identity").expect("legacy identity should write");
        std::fs::write(
            legacy_config_dir.join(INSTANCE_STORE_FILE_NAME),
            "legacy config",
        )
        .expect("legacy instance store should write");
        std::fs::create_dir_all(
            legacy_sync_root_identity
                .parent()
                .expect("legacy sync-root identity should have a parent"),
        )
        .expect("legacy sync-root identity parent should create");
        std::fs::write(&legacy_sync_root_identity, "legacy sync-root identity")
            .expect("legacy sync-root identity should write");
        std::fs::create_dir_all(&external_state_dir)
            .expect("external state directory should create");
        std::fs::write(
            external_state_dir.join("thumbnail-provider.log"),
            "external thumbnail log",
        )
        .expect("external thumbnail log should write");

        assert!(
            migrate_legacy_virtualized_state_dir(&legacy_state_dir, &external_state_dir)
                .expect("legacy state should migrate")
        );
        assert_eq!(
            std::fs::read_to_string(external_config_dir.join(INSTANCE_STORE_FILE_NAME))
                .expect("migrated instance store should read"),
            "legacy config"
        );
        assert_eq!(
            std::fs::read_to_string(
                external_config_dir
                    .join("client-identities")
                    .join("identity.json")
            )
            .expect("migrated identity should read"),
            "legacy identity"
        );
        assert_eq!(
            std::fs::read_to_string(
                external_state_dir
                    .join("sync-roots")
                    .join("atuh-main")
                    .join("client-identity.json")
            )
            .expect("migrated sync-root identity should read"),
            "legacy sync-root identity"
        );
        assert_eq!(
            std::fs::read_to_string(external_state_dir.join("thumbnail-provider.log"))
                .expect("external thumbnail log should remain readable"),
            "external thumbnail log"
        );

        std::fs::write(
            external_config_dir.join(INSTANCE_STORE_FILE_NAME),
            "external config",
        )
        .expect("external instance store should overwrite");
        assert!(
            !migrate_legacy_virtualized_state_dir(&legacy_state_dir, &external_state_dir)
                .expect("existing external state should be left intact")
        );
        assert_eq!(
            std::fs::read_to_string(external_config_dir.join(INSTANCE_STORE_FILE_NAME))
                .expect("external instance store should remain readable"),
            "external config"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_virtualized_state_migration_skips_while_another_process_holds_the_lock() {
        let path = temp_path("legacy-virtualized-state-lock");
        let root = path.parent().expect("temp path should have parent");
        let legacy_state_dir = root.join("legacy").join(LOCAL_STATE_ROOT_DIR);
        let external_state_dir = root.join("external").join(LOCAL_STATE_ROOT_DIR);
        std::fs::create_dir_all(&legacy_state_dir).expect("legacy state directory should create");
        std::fs::write(legacy_state_dir.join("state.json"), "legacy state")
            .expect("legacy state should write");

        let lock_path =
            migration_lock_path(&external_state_dir).expect("migration lock path should resolve");
        std::fs::create_dir_all(lock_path.parent().expect("lock should have a parent"))
            .expect("migration lock parent should create");
        std::fs::write(&lock_path, std::process::id().to_string())
            .expect("migration lock should write");

        assert!(
            !migrate_legacy_virtualized_state_dir(&legacy_state_dir, &external_state_dir)
                .expect("active migration lock should skip migration")
        );
        assert!(
            !external_state_dir.exists(),
            "active migration lock must prevent publishing any state"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn package_family_name_is_derived_from_an_msix_package_full_name() {
        assert_eq!(
            package_family_name_from_full_name(
                "UlrichHornung.IronMesh_2.0.1.0_neutral__bnh81bg69mtt8",
            ),
            Some("UlrichHornung.IronMesh_bnh81bg69mtt8".to_string())
        );
        assert_eq!(package_family_name_from_full_name("berrykeep"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn background_child_reaper_collects_exited_process() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("short-lived child should spawn");
        let pid = child.id();

        spawn_background_child_reaper(child, "test-child".to_string(), None);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !Path::new("/proc").join(pid.to_string()).exists() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }

        panic!("child pid {pid} was not reaped within the timeout window");
    }
}
