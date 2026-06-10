#![cfg(windows)]

#[path = "../framework.rs"]
mod framework;
#[path = "../framework_win.rs"]
mod framework_win;
#[path = "../windows_cluster_workload_support.rs"]
mod windows_cluster_workload_support;
#[path = "../windows_folder_agent_cluster_workload_support.rs"]
mod windows_folder_agent_cluster_workload_support;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    windows_folder_agent_cluster_workload_support::run_live_driver_from_env().await
}
