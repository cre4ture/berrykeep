use std::{
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use patchbay::Device;
use tokio::{
    process::{Child, Command},
    time::timeout,
};
use uuid::Uuid;

use super::tls::NodeTlsPaths;

pub(super) const ADMIN_TOKEN: &str = "quic-network-test-admin";
pub(super) const RENDEZVOUS_PORT: u16 = 19_090;
pub(super) const NODE_PUBLIC_PORT: u16 = 18_080;
pub(super) const NODE_INTERNAL_PORT: u16 = 28_080;
pub(super) const CLI_WEB_PORT: u16 = 18_081;
pub(super) const PROCESS_READY_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) struct ProcessGuard {
    name: &'static str,
    child: Option<Child>,
    stderr_path: PathBuf,
}

impl ProcessGuard {
    fn new(name: &'static str, child: Child, stderr_path: PathBuf) -> Self {
        Self {
            name,
            child: Some(child),
            stderr_path,
        }
    }

    pub(super) fn ensure_running(&mut self) -> Result<()> {
        if let Some(status) = self
            .child
            .as_mut()
            .context("process guard has no child")?
            .try_wait()
            .with_context(|| format!("failed querying {} process", self.name))?
        {
            bail!(
                "{} exited early with {status}\n{}",
                self.name,
                self.stderr_tail()
            );
        }
        Ok(())
    }

    pub(super) fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    pub(super) fn stderr_tail(&self) -> String {
        log_tail(&self.stderr_path)
    }

    pub(super) async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

pub(super) fn spawn_rendezvous(
    device: &Device,
    artifacts: &Path,
    public_url: &str,
    iroh_relay_enabled: bool,
) -> Result<ProcessGuard> {
    let stdout_path = artifacts.join("rendezvous.stdout.log");
    let stderr_path = artifacts.join("rendezvous.stderr.log");
    let mut command = Command::new(binary_path("rendezvous-service")?);
    isolate_network_environment(&mut command);
    command
        .env(
            "IRONMESH_RENDEZVOUS_BIND",
            format!("0.0.0.0:{RENDEZVOUS_PORT}"),
        )
        .env("IRONMESH_RENDEZVOUS_PUBLIC_URL", public_url)
        .env("IRONMESH_RENDEZVOUS_ALLOW_INSECURE_HTTP", "true")
        .env(
            "IRONMESH_IROH_RELAY_ENABLED",
            if iroh_relay_enabled { "true" } else { "false" },
        )
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let child = device
        .spawn_command(command)
        .context("failed spawning rendezvous service in namespace")?;
    Ok(ProcessGuard::new("rendezvous-service", child, stderr_path))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_node(
    device: &Device,
    artifacts: &Path,
    rendezvous_url: &str,
    cluster_id: Uuid,
    node_id: Uuid,
    node_ip: Ipv4Addr,
    tls: &NodeTlsPaths,
) -> Result<ProcessGuard> {
    let stdout_path = artifacts.join("node.stdout.log");
    let stderr_path = artifacts.join("node.stderr.log");
    let data_dir = artifacts.join("node-data");
    fs::create_dir_all(&data_dir)?;
    let mut command = Command::new(binary_path("server-node")?);
    isolate_network_environment(&mut command);
    command
        .env(
            "IRONMESH_SERVER_BIND",
            format!("0.0.0.0:{NODE_PUBLIC_PORT}"),
        )
        .env(
            "IRONMESH_PUBLIC_URL",
            format!("http://{node_ip}:{NODE_PUBLIC_PORT}"),
        )
        .env("IRONMESH_DATA_DIR", data_dir)
        .env("IRONMESH_CLUSTER_ID", cluster_id.to_string())
        .env("IRONMESH_NODE_ID", node_id.to_string())
        .env(
            "IRONMESH_INTERNAL_BIND",
            format!("0.0.0.0:{NODE_INTERNAL_PORT}"),
        )
        .env(
            "IRONMESH_INTERNAL_URL",
            format!("https://{node_ip}:{NODE_INTERNAL_PORT}"),
        )
        .env("IRONMESH_INTERNAL_TLS_CA_CERT", &tls.ca_cert)
        .env("IRONMESH_INTERNAL_TLS_CA_KEY", &tls.ca_key)
        .env("IRONMESH_INTERNAL_TLS_CERT", &tls.node_cert)
        .env("IRONMESH_INTERNAL_TLS_KEY", &tls.node_key)
        .env("IRONMESH_RENDEZVOUS_URLS", rendezvous_url)
        .env("IRONMESH_RELAY_MODE", "fallback")
        .env("IRONMESH_PUBLIC_PEER_API_ENABLED", "true")
        .env("IRONMESH_REQUIRE_CLIENT_AUTH", "true")
        .env("IRONMESH_ALLOW_UNAUTHENTICATED_CLIENTS", "true")
        .env("IRONMESH_ALLOW_INSECURE_PUBLIC_HTTP", "true")
        .env("IRONMESH_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("IRONMESH_REPLICATION_FACTOR", "1")
        .env("IRONMESH_AUTONOMOUS_HEARTBEAT_ENABLED", "false")
        .env("IRONMESH_AUTONOMOUS_REPLICATION_ON_PUT_ENABLED", "false")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let child = device
        .spawn_command(command)
        .context("failed spawning server node in namespace")?;
    Ok(ProcessGuard::new("server-node", child, stderr_path))
}

pub(super) async fn enroll_cli(device: &Device, bootstrap: &Path, identity: &Path) -> Result<()> {
    let mut command = Command::new(binary_path("cli-client")?);
    isolate_network_environment(&mut command);
    command
        .arg("--bootstrap-file")
        .arg(bootstrap)
        .arg("--client-identity-file")
        .arg(identity)
        .arg("enroll")
        .arg("--label")
        .arg("quic-network-cli");
    let child = device
        .spawn_command(command)
        .context("failed spawning CLI enrollment in namespace")?;
    let output = timeout(PROCESS_READY_TIMEOUT, child.wait_with_output())
        .await
        .context("CLI enrollment timed out")?
        .context("failed waiting for CLI enrollment")?;
    ensure!(
        output.status.success(),
        "CLI enrollment failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(identity.exists(), "CLI enrollment did not write identity");
    Ok(())
}

pub(super) fn spawn_cli_web(
    device: &Device,
    artifacts: &Path,
    bootstrap: &Path,
    identity: &Path,
) -> Result<ProcessGuard> {
    let stdout_path = artifacts.join("cli.stdout.log");
    let stderr_path = artifacts.join("cli.stderr.log");
    let mut command = Command::new(binary_path("cli-client")?);
    isolate_network_environment(&mut command);
    command
        .arg("--bootstrap-file")
        .arg(bootstrap)
        .arg("--client-identity-file")
        .arg(identity)
        .arg("serve-web")
        .arg("--bind")
        .arg(format!("0.0.0.0:{CLI_WEB_PORT}"))
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let child = device
        .spawn_command(command)
        .context("failed spawning CLI web server in namespace")?;
    Ok(ProcessGuard::new("cli-client", child, stderr_path))
}

pub(super) fn artifact_dir(scenario: &str) -> Result<PathBuf> {
    let root = std::env::var_os("IRONMESH_QUIC_TEST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("ironmesh-quic-network-tests"));
    let path = root.join(format!("{scenario}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path)
        .with_context(|| format!("failed creating artifact directory {}", path.display()))?;
    Ok(path)
}

fn binary_path(name: &str) -> Result<PathBuf> {
    let artifact = match name {
        "server-node" => option_env!("CARGO_BIN_FILE_SERVER_NODE_ironmesh-server-node"),
        "cli-client" => option_env!("CARGO_BIN_FILE_CLI_CLIENT_ironmesh"),
        "rendezvous-service" => {
            option_env!("CARGO_BIN_FILE_RENDEZVOUS_SERVICE_ironmesh-rendezvous-service")
        }
        _ => None,
    };
    if let Some(path) = artifact {
        return Ok(PathBuf::from(path));
    }

    let executable = match name {
        "server-node" => "ironmesh-server-node",
        "cli-client" => "ironmesh",
        "rendezvous-service" => "ironmesh-rendezvous-service",
        other => other,
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/debug")
        .join(executable);
    ensure!(
        path.exists(),
        "missing {name} binary {}; use nightly Cargo artifact dependencies",
        path.display()
    );
    Ok(path)
}

fn isolate_network_environment(command: &mut Command) {
    command.env("NO_PROXY", "*").env("no_proxy", "*");
}

fn log_tail(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(120)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}
