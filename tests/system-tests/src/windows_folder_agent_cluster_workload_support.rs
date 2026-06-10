use anyhow::Result;

use crate::windows_cluster_workload_support::{
    LocalRuntimeKind, run_live_driver_from_env_for, run_managed_test_workload_for,
};

pub async fn run_managed_test_workload() -> Result<()> {
    run_managed_test_workload_for(LocalRuntimeKind::FolderAgent).await
}

pub async fn run_live_driver_from_env() -> Result<()> {
    run_live_driver_from_env_for(LocalRuntimeKind::FolderAgent).await
}
