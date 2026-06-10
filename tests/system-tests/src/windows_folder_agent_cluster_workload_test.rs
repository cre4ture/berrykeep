#![cfg(windows)]

#[cfg(test)]
mod tests {
    use crate::windows_folder_agent_cluster_workload_support::run_managed_test_workload;
    use anyhow::Result;

    #[tokio::test]
    #[ignore = "expensive local Windows Folder Agent cluster workload"]
    async fn windows_folder_agent_cluster_upload_and_replication_workload() -> Result<()> {
        run_managed_test_workload().await
    }
}
