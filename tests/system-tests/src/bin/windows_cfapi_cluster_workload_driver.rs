#[cfg(windows)]
#[path = "../framework.rs"]
mod framework;
#[cfg(windows)]
#[path = "../framework_win.rs"]
mod framework_win;
#[cfg(windows)]
#[path = "../windows_cfapi_cluster_workload_support.rs"]
mod windows_cfapi_cluster_workload_support;
#[cfg(windows)]
#[path = "../windows_cluster_workload_support.rs"]
mod windows_cluster_workload_support;

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    windows_cfapi_cluster_workload_support::run_live_driver_from_env().await
}

#[cfg(not(windows))]
fn main() {}
