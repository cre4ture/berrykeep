use anyhow::Result;
use clap::Parser;

const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_INFO: &str = git_version::git_version!(
    prefix = "Build revision: ",
    fallback = "Build revision: unknown",
    args = ["--tags", "--always", "--dirty=-dirty", "--abbrev=12"]
);
const LONG_VERSION: &str = git_version::git_version!(
    prefix = concat!(env!("CARGO_PKG_VERSION"), "\nBuild revision: "),
    fallback = concat!(env!("CARGO_PKG_VERSION"), "\nBuild revision: unknown"),
    args = ["--tags", "--always", "--dirty=-dirty", "--abbrev=12"]
);

#[derive(Debug, Parser)]
#[command(name = "ironmesh-server-node")]
#[command(about = "BerryKeep server node")]
#[command(version = PACKAGE_VERSION)]
#[command(long_version = LONG_VERSION)]
#[command(after_help = BUILD_INFO)]
struct Cli {
    /// Run under the Windows Service Control Manager.
    #[arg(long, hide = true)]
    windows_service: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    #[cfg(windows)]
    if cli.windows_service {
        windows_service_host::load_environment_file()?;
        return windows_service_host::run();
    }

    #[cfg(not(windows))]
    if cli.windows_service {
        anyhow::bail!("--windows-service is only available on Windows");
    }

    run_server_node()
}

fn run_server_node() -> Result<()> {
    build_runtime()?.block_on(server_node_sdk::run_from_env())
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    let use_current_thread =
        std::env::var_os("IRONMESH_TOKIO_CURRENT_THREAD").is_some_and(|v| v != "0");

    let mut runtime_builder = if use_current_thread {
        eprintln!(
            "ironmesh-server-node: using Tokio current-thread runtime because \
IRONMESH_TOKIO_CURRENT_THREAD is set; this avoids worker-pool overhead on \
single-core hosts"
        );
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    runtime_builder.enable_all();

    Ok(runtime_builder.build()?)
}

#[cfg(windows)]
mod windows_service_host {
    use super::*;
    use anyhow::{Context, anyhow, bail};
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::watch;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    const SERVICE_NAME: &str = "BerryKeepServerNode";
    const SERVICE_START_WAIT_HINT: Duration = Duration::from_secs(30);
    const SERVICE_STOP_WAIT_HINT: Duration = Duration::from_secs(30);

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub fn load_environment_file() -> Result<()> {
        let path = service_environment_file_path();
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "failed reading Windows service environment file {}",
                path.display()
            )
        })?;

        for (line_number, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (name, value) = line.split_once('=').ok_or_else(|| {
                anyhow!(
                    "invalid Windows service environment entry at {}:{}; expected NAME=VALUE",
                    path.display(),
                    line_number + 1
                )
            })?;
            let name = name.trim();
            let value = value.trim();
            if !is_allowed_environment_name(name) {
                bail!(
                    "unsupported Windows service environment variable {name:?} at {}:{}",
                    path.display(),
                    line_number + 1
                );
            }

            // This is the only environment mutation in the process and it happens
            // before the Service Control Manager starts the service thread or a
            // Tokio runtime is created. That satisfies `set_var`'s process-wide
            // safety requirement in Rust 2024.
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(name, value);
            }
        }

        Ok(())
    }

    pub fn run() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main).context(
            "failed connecting BerryKeep Server Node to the Windows Service Control Manager",
        )
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            eprintln!("BerryKeep Server Node service failed: {error:#}");
        }
    }

    fn run_service() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let status_handle_slot: Arc<Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
            Arc::new(Mutex::new(None));
        let status_handle_for_events = Arc::clone(&status_handle_slot);
        let shutdown_tx_for_events = shutdown_tx.clone();

        let event_handler = move |control_event| match control_event {
            ServiceControl::Stop => {
                if let Ok(status_handle) = status_handle_for_events.lock()
                    && let Some(status_handle) = *status_handle
                {
                    let _ = status_handle.set_service_status(service_status(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        SERVICE_STOP_WAIT_HINT,
                        1,
                        0,
                    ));
                }
                let _ = shutdown_tx_for_events.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("failed registering BerryKeep Server Node service control handler")?;
        *status_handle_slot
            .lock()
            .map_err(|_| anyhow!("Windows service status lock poisoned"))? = Some(status_handle);
        status_handle
            .set_service_status(service_status(
                ServiceState::StartPending,
                ServiceControlAccept::empty(),
                SERVICE_START_WAIT_HINT,
                1,
                0,
            ))
            .context("failed reporting BerryKeep Server Node service start")?;

        let result = (|| -> Result<()> {
            let runtime = build_runtime()?;
            status_handle
                .set_service_status(service_status(
                    ServiceState::Running,
                    ServiceControlAccept::STOP,
                    Duration::default(),
                    0,
                    0,
                ))
                .context("failed reporting BerryKeep Server Node service running")?;
            runtime.block_on(server_node_sdk::run_from_env_with_shutdown(shutdown_rx))
        })();

        let error_code = u32::from(result.is_err());
        status_handle
            .set_service_status(service_status(
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                Duration::default(),
                0,
                error_code,
            ))
            .context("failed reporting BerryKeep Server Node service stopped")?;
        result
    }

    fn service_status(
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
        wait_hint: Duration,
        checkpoint: u32,
        exit_code: u32,
    ) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint,
            wait_hint,
            process_id: None,
        }
    }

    fn service_environment_file_path() -> PathBuf {
        let program_data = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        program_data
            .join("BerryKeep")
            .join("ServerNode")
            .join("server-node.env")
    }

    fn is_allowed_environment_name(name: &str) -> bool {
        name == "RUST_LOG"
            || name == "BERRYKEEP_SERVER_NODE_DATA_DIR"
            || name == "BERRYKEEP_SERVER_NODE_BIND"
            || name.starts_with("IRONMESH_")
    }

    #[cfg(test)]
    mod tests {
        use super::is_allowed_environment_name;

        #[test]
        fn service_environment_allows_node_settings_only() {
            assert!(is_allowed_environment_name("BERRYKEEP_SERVER_NODE_BIND"));
            assert!(is_allowed_environment_name("IRONMESH_RENDEZVOUS_URLS"));
            assert!(is_allowed_environment_name("RUST_LOG"));
            assert!(!is_allowed_environment_name("PATH"));
            assert!(!is_allowed_environment_name("BERRYKEEP_UNRELATED"));
        }
    }
}
