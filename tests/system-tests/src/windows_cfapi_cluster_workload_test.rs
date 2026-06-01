#![cfg(windows)]

#[cfg(test)]
mod tests {
    use crate::framework::{
        fresh_data_dir, internal_base_url_from_public_bind,
        issue_bootstrap_bundle_and_enroll_client, mtls_client_from_data_dir, register_node,
        start_authenticated_server_with_env_options, stop_server, wait_for_online_nodes,
        ChildGuard, TEST_ADMIN_TOKEN,
    };
    use crate::framework_win::start_cfapi_adapter_with_bootstrap;
    use anyhow::{bail, Context, Result};
    use blake3::Hash;
    use client_sdk::ironmesh_client::StoreIndexRequestOptions;
    use client_sdk::IronMeshClient;
    use reqwest::Client;
    use std::collections::BTreeSet;
    use std::fs::{self, File};
    use std::io::{BufWriter, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tokio::time::sleep;
    use uuid::Uuid;

    const DEFAULT_FILE_COUNT: usize = 4_000;
    const DEFAULT_MIN_BYTES: usize = 1 * 1024 * 1024;
    const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
    const DEFAULT_SAMPLE_VERIFY_COUNT: usize = 24;
    const DEFAULT_SUBDIR_COUNT: usize = 80;
    const DEFAULT_MAX_DIR_DEPTH: usize = 4;
    const STORE_INDEX_PAGE_SIZE: usize = 256;
    const IO_BUFFER_BYTES: usize = 256 * 1024;
    const UPLOAD_TIMEOUT: Duration = Duration::from_secs(45 * 60);
    const REPLICATION_TIMEOUT: Duration = Duration::from_secs(60 * 60);

    #[derive(Debug, Clone)]
    struct WorkloadConfig {
        file_count: usize,
        min_bytes: usize,
        max_bytes: usize,
        sample_verify_count: usize,
        subdir_count: usize,
        max_dir_depth: usize,
    }

    impl WorkloadConfig {
        fn from_env() -> Result<Self> {
            let file_count =
                read_env_usize("IRONMESH_WINDOWS_CFAPI_LOAD_FILE_COUNT", DEFAULT_FILE_COUNT)?;
            let min_bytes =
                read_env_usize("IRONMESH_WINDOWS_CFAPI_LOAD_MIN_BYTES", DEFAULT_MIN_BYTES)?;
            let max_bytes =
                read_env_usize("IRONMESH_WINDOWS_CFAPI_LOAD_MAX_BYTES", DEFAULT_MAX_BYTES)?;
            let sample_verify_count = read_env_usize(
                "IRONMESH_WINDOWS_CFAPI_LOAD_VERIFY_SAMPLE_COUNT",
                DEFAULT_SAMPLE_VERIFY_COUNT,
            )?;
            let subdir_count = read_env_usize(
                "IRONMESH_WINDOWS_CFAPI_LOAD_SUBDIR_COUNT",
                DEFAULT_SUBDIR_COUNT,
            )?;
            let max_dir_depth = read_env_usize(
                "IRONMESH_WINDOWS_CFAPI_LOAD_MAX_DIR_DEPTH",
                DEFAULT_MAX_DIR_DEPTH,
            )?;

            if min_bytes == 0 {
                bail!("IRONMESH_WINDOWS_CFAPI_LOAD_MIN_BYTES must be greater than zero");
            }
            if max_bytes < min_bytes {
                bail!("IRONMESH_WINDOWS_CFAPI_LOAD_MAX_BYTES must be >= MIN_BYTES");
            }
            if file_count == 0 {
                bail!("IRONMESH_WINDOWS_CFAPI_LOAD_FILE_COUNT must be greater than zero");
            }
            if subdir_count == 0 {
                bail!("IRONMESH_WINDOWS_CFAPI_LOAD_SUBDIR_COUNT must be greater than zero");
            }
            if max_dir_depth == 0 {
                bail!("IRONMESH_WINDOWS_CFAPI_LOAD_MAX_DIR_DEPTH must be greater than zero");
            }
            if subdir_count > file_count {
                bail!(
                    "IRONMESH_WINDOWS_CFAPI_LOAD_SUBDIR_COUNT must be <= FILE_COUNT so each subdir can receive at least one file"
                );
            }

            Ok(Self {
                file_count,
                min_bytes,
                max_bytes,
                sample_verify_count: sample_verify_count.max(1),
                subdir_count,
                max_dir_depth,
            })
        }

        fn average_bytes(&self) -> usize {
            self.min_bytes.saturating_add(self.max_bytes) / 2
        }
    }

    #[derive(Debug, Clone)]
    struct FileSpec {
        relative_path: PathBuf,
        store_path: String,
        size_bytes: usize,
        content_hash: Hash,
    }

    struct ClusterNodeFixture {
        label: &'static str,
        node_id: String,
        base_url: String,
        internal_base_url: String,
        internal_http: Client,
        data_dir: PathBuf,
        client_dir: PathBuf,
        bootstrap_file: PathBuf,
        sdk: IronMeshClient,
        server: ChildGuard,
    }

    impl ClusterNodeFixture {
        async fn stop_and_cleanup(&mut self) {
            stop_server(&mut self.server).await;
            let _ = fs::remove_dir_all(&self.data_dir);
            let _ = fs::remove_dir_all(&self.client_dir);
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct XorShift64 {
        state: u64,
    }

    impl XorShift64 {
        fn new(seed: u64) -> Self {
            let state = if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            };
            Self { state }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }
    }

    fn normalized_store_path(relative_path: &Path) -> Result<String> {
        let components = relative_path
            .iter()
            .map(|component| {
                component.to_str().with_context(|| {
                    format!(
                        "relative path {} contains a non-utf8 component",
                        relative_path.display()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(components.join("/"))
    }

    fn read_env_usize(name: &str, default: usize) -> Result<usize> {
        match std::env::var(name) {
            Ok(value) => value
                .parse::<usize>()
                .with_context(|| format!("failed parsing {name}={value} as usize")),
            Err(std::env::VarError::NotPresent) => Ok(default),
            Err(err) => Err(err).with_context(|| format!("failed reading {name}")),
        }
    }

    fn file_size_for_index(config: &WorkloadConfig, index: usize) -> usize {
        let range = config.max_bytes - config.min_bytes + 1;
        let mix = 0xA076_1D64_78BD_642Fu64.wrapping_mul((index as u64).wrapping_add(1));
        config.min_bytes + (mix as usize % range)
    }

    fn file_seed_for_index(index: usize) -> u64 {
        0xD1B5_4A32_D192_ED03u64 ^ ((index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn build_directory_layout(config: &WorkloadConfig) -> Vec<PathBuf> {
        let mut rng = XorShift64::new(0x8D51_AA23_7F34_9C17);
        let mut directories: Vec<PathBuf> = Vec::with_capacity(config.subdir_count);
        let mut by_depth = vec![Vec::<usize>::new(); config.max_dir_depth + 1];

        for dir_index in 0..config.subdir_count {
            let desired_depth = if dir_index < config.max_dir_depth {
                dir_index + 1
            } else {
                1 + (rng.next_u64() as usize % config.max_dir_depth)
            };
            let parent_path = if desired_depth == 1 || by_depth[desired_depth - 1].is_empty() {
                PathBuf::new()
            } else {
                let candidates = &by_depth[desired_depth - 1];
                directories[candidates[rng.next_u64() as usize % candidates.len()]].clone()
            };
            let path = parent_path.join(format!("dir-{dir_index:03}"));
            by_depth[desired_depth].push(directories.len());
            directories.push(path);
        }

        directories
    }

    fn fill_pseudorandom(buffer: &mut [u8], rng: &mut XorShift64) {
        let mut offset = 0usize;
        while offset + 8 <= buffer.len() {
            buffer[offset..offset + 8].copy_from_slice(&rng.next_u64().to_le_bytes());
            offset += 8;
        }
        if offset < buffer.len() {
            let tail = rng.next_u64().to_le_bytes();
            let remaining = buffer.len() - offset;
            buffer[offset..].copy_from_slice(&tail[..remaining]);
        }
    }

    fn write_random_file(path: &Path, size_bytes: usize, seed: u64) -> Result<Hash> {
        let file = File::create(path)
            .with_context(|| format!("failed to create staged file {}", path.display()))?;
        let mut writer = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
        let mut buffer = vec![0u8; IO_BUFFER_BYTES];
        let mut rng = XorShift64::new(seed);
        let mut hasher = blake3::Hasher::new();
        let mut remaining = size_bytes;

        while remaining > 0 {
            let chunk_len = remaining.min(buffer.len());
            fill_pseudorandom(&mut buffer[..chunk_len], &mut rng);
            writer
                .write_all(&buffer[..chunk_len])
                .with_context(|| format!("failed writing staged file {}", path.display()))?;
            hasher.update(&buffer[..chunk_len]);
            remaining -= chunk_len;
        }

        writer
            .flush()
            .with_context(|| format!("failed flushing staged file {}", path.display()))?;
        writer
            .into_inner()
            .with_context(|| format!("failed finalizing staged file {}", path.display()))?
            .sync_all()
            .with_context(|| format!("failed syncing staged file {}", path.display()))?;
        Ok(hasher.finalize())
    }

    async fn start_cluster_node(
        bind: &str,
        label: &'static str,
        node_id: &str,
        cluster_id: &str,
        replication_factor: usize,
    ) -> Result<ClusterNodeFixture> {
        let data_dir = fresh_data_dir(&format!("{label}-server"));
        let client_dir = fresh_data_dir(&format!("{label}-client"));
        let env = [
            ("IRONMESH_CLUSTER_ID", cluster_id),
            ("IRONMESH_AUTONOMOUS_HEARTBEAT_ENABLED", "true"),
        ];

        let server = start_authenticated_server_with_env_options(
            bind,
            &data_dir,
            node_id,
            replication_factor,
            None,
            Some(60 * 60),
            &env,
        )
        .await?;

        let base_url = format!("http://{bind}");
        let internal_base_url = internal_base_url_from_public_bind(bind)?;
        let internal_http = mtls_client_from_data_dir(&data_dir)?;
        let enrolled = issue_bootstrap_bundle_and_enroll_client(
            &Client::new(),
            &base_url,
            TEST_ADMIN_TOKEN,
            &client_dir,
            &format!("{label}.bootstrap.json"),
            Some(label),
            Some(12 * 60 * 60),
        )
        .await?;
        let sdk = enrolled.build_client_async().await?;

        Ok(ClusterNodeFixture {
            label,
            node_id: node_id.to_string(),
            base_url,
            internal_base_url,
            internal_http,
            data_dir,
            client_dir,
            bootstrap_file: enrolled.bootstrap_path,
            sdk,
            server,
        })
    }

    async fn register_full_mesh(http: &Client, nodes: &[&ClusterNodeFixture]) -> Result<()> {
        for (controller_index, controller) in nodes.iter().enumerate() {
            for (peer_index, peer) in nodes.iter().enumerate() {
                if controller_index == peer_index {
                    continue;
                }
                let dc = match peer_index {
                    0 => "dc-a",
                    1 => "dc-b",
                    _ => "dc-c",
                };
                let rack = match peer_index {
                    0 => "rack-a",
                    1 => "rack-b",
                    _ => "rack-c",
                };
                register_node(
                    http,
                    &controller.base_url,
                    &peer.node_id,
                    &peer.base_url,
                    dc,
                    rack,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn fetch_all_store_file_paths(sdk: &IronMeshClient) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        let mut offset = 0usize;

        loop {
            let response = sdk
                .store_index_with_options(
                    None,
                    8,
                    None,
                    StoreIndexRequestOptions {
                        offset: Some(offset),
                        limit: Some(STORE_INDEX_PAGE_SIZE),
                        ..StoreIndexRequestOptions::default()
                    },
                )
                .await?;

            let batch_len = response.entries.len();
            for entry in response.entries {
                if !entry.path.ends_with('/') {
                    paths.insert(entry.path);
                }
            }

            if batch_len == 0 || !response.has_more {
                break;
            }
            offset += batch_len;
        }

        Ok(paths)
    }

    async fn wait_for_store_file_paths(
        sdk: &IronMeshClient,
        expected_paths: &BTreeSet<String>,
        label: &str,
        timeout: Duration,
    ) -> Result<()> {
        let started = Instant::now();
        let mut last_log = Instant::now();
        loop {
            match fetch_all_store_file_paths(sdk).await {
                Ok(actual_paths) if actual_paths == *expected_paths => return Ok(()),
                Ok(actual_paths) => {
                    if last_log.elapsed() >= Duration::from_secs(10) {
                        let missing = expected_paths
                            .difference(&actual_paths)
                            .take(5)
                            .cloned()
                            .collect::<Vec<_>>();
                        let extra = actual_paths
                            .difference(expected_paths)
                            .take(5)
                            .cloned()
                            .collect::<Vec<_>>();
                        eprintln!(
                            "[{label}] store index progress: have={} expected={} missing_sample={missing:?} extra_sample={extra:?}",
                            actual_paths.len(),
                            expected_paths.len()
                        );
                        last_log = Instant::now();
                    }
                }
                Err(err) if last_log.elapsed() >= Duration::from_secs(10) => {
                    eprintln!("[{label}] store index retry after error: {err:#}");
                    last_log = Instant::now();
                }
                Err(_) => {}
            }

            if started.elapsed() >= timeout {
                let actual_paths = fetch_all_store_file_paths(sdk).await.unwrap_or_default();
                let missing = expected_paths
                    .difference(&actual_paths)
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>();
                bail!(
                    "[{label}] timed out waiting for store index convergence: have={} expected={} missing_sample={missing:?}",
                    actual_paths.len(),
                    expected_paths.len()
                );
            }

            sleep(Duration::from_secs(2)).await;
        }
    }

    async fn local_available_subjects(http: &Client, base_url: &str) -> Result<BTreeSet<String>> {
        let payload = http
            .get(format!("{base_url}/cluster/availability/subjects/local"))
            .header("x-ironmesh-admin-token", TEST_ADMIN_TOKEN)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        Ok(payload
            .get("subjects")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(ToString::to_string))
            .collect())
    }

    async fn wait_for_local_subjects(
        http: &Client,
        base_url: &str,
        expected_paths: &BTreeSet<String>,
        label: &str,
        timeout: Duration,
    ) -> Result<()> {
        let started = Instant::now();
        let mut last_log = Instant::now();
        loop {
            match local_available_subjects(http, base_url).await {
                Ok(subjects) => {
                    let missing = expected_paths
                        .iter()
                        .filter(|path| !subjects.contains(*path))
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>();
                    if missing.is_empty() {
                        return Ok(());
                    }

                    if last_log.elapsed() >= Duration::from_secs(10) {
                        eprintln!(
                            "[{label}] local availability progress: available={} expected={} missing_sample={missing:?}",
                            subjects.len(),
                            expected_paths.len()
                        );
                        last_log = Instant::now();
                    }
                }
                Err(err) if last_log.elapsed() >= Duration::from_secs(10) => {
                    eprintln!("[{label}] local availability retry after error: {err:#}");
                    last_log = Instant::now();
                }
                Err(_) => {}
            }

            if started.elapsed() >= timeout {
                let subjects = local_available_subjects(http, base_url)
                    .await
                    .unwrap_or_default();
                let missing = expected_paths
                    .iter()
                    .filter(|path| !subjects.contains(*path))
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>();
                bail!(
                    "[{label}] timed out waiting for local availability convergence: available={} expected={} missing_sample={missing:?}",
                    subjects.len(),
                    expected_paths.len()
                );
            }

            sleep(Duration::from_secs(2)).await;
        }
    }

    async fn current_under_replicated(http: &Client, base_url: &str) -> Result<u64> {
        let payload = http
            .get(format!("{base_url}/cluster/replication/plan"))
            .header("x-ironmesh-admin-token", TEST_ADMIN_TOKEN)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        payload
            .get("under_replicated")
            .and_then(|value| value.as_u64())
            .context("replication plan response missing under_replicated")
    }

    async fn drive_replication_to_completion(
        http: &Client,
        base_url: &str,
        timeout: Duration,
    ) -> Result<()> {
        let started = Instant::now();
        let mut last_repair = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .unwrap_or_else(Instant::now);

        loop {
            let under_replicated = current_under_replicated(http, base_url).await?;
            if under_replicated == 0 {
                return Ok(());
            }

            if last_repair.elapsed() >= Duration::from_secs(15) {
                let report = http
                    .post(format!("{base_url}/cluster/replication/repair"))
                    .header("x-ironmesh-admin-token", TEST_ADMIN_TOKEN)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<serde_json::Value>()
                    .await?;
                let successful = report
                    .get("successful_transfers")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let failed = report
                    .get("failed_transfers")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                eprintln!(
                    "[cluster] repair pass: under_replicated={under_replicated} successful_transfers={successful} failed_transfers={failed}"
                );
                last_repair = Instant::now();
            }

            if started.elapsed() >= timeout {
                bail!(
                    "timed out waiting for replication repair to finish at {base_url}; under_replicated={under_replicated}"
                );
            }

            sleep(Duration::from_secs(5)).await;
        }
    }

    fn select_sample_specs(file_specs: &[FileSpec], sample_count: usize) -> Vec<FileSpec> {
        if file_specs.len() <= sample_count {
            return file_specs.to_vec();
        }

        let last_index = file_specs.len() - 1;
        let mut indices = BTreeSet::new();
        for slot in 0..sample_count {
            let index = if sample_count == 1 {
                0
            } else {
                slot.saturating_mul(last_index) / (sample_count - 1)
            };
            indices.insert(index);
        }

        indices
            .into_iter()
            .filter_map(|index| file_specs.get(index).cloned())
            .collect()
    }

    async fn verify_sample_content(
        sdk: &IronMeshClient,
        label: &str,
        sample_specs: &[FileSpec],
    ) -> Result<()> {
        for (index, spec) in sample_specs.iter().enumerate() {
            let bytes = sdk
                .get(&spec.store_path)
                .await
                .with_context(|| format!("[{label}] failed to fetch {}", spec.store_path))?;
            let hash = blake3::hash(bytes.as_ref());
            if bytes.len() != spec.size_bytes {
                bail!(
                    "[{label}] size mismatch for {}: expected={} actual={}",
                    spec.store_path,
                    spec.size_bytes,
                    bytes.len()
                );
            }
            if hash != spec.content_hash {
                bail!(
                    "[{label}] hash mismatch for {}: expected={} actual={}",
                    spec.store_path,
                    spec.content_hash.to_hex(),
                    hash.to_hex()
                );
            }

            if (index + 1) % 4 == 0 || index + 1 == sample_specs.len() {
                eprintln!(
                    "[{label}] verified {}/{} sampled files",
                    index + 1,
                    sample_specs.len()
                );
            }
        }

        Ok(())
    }

    fn stage_workload(config: &WorkloadConfig, source_dir: &Path) -> Result<Vec<FileSpec>> {
        fs::create_dir_all(source_dir)
            .with_context(|| format!("failed to create {}", source_dir.display()))?;
        let mut specs = Vec::with_capacity(config.file_count);
        let directory_layout = build_directory_layout(config);
        let mut directory_rng = XorShift64::new(0x61C8_A1D4_08E7_395B);
        let mut total_bytes = 0usize;

        for index in 0..config.file_count {
            let file_name = format!("load-{index:05}.bin");
            let directory_index = if index < directory_layout.len() {
                index
            } else {
                directory_rng.next_u64() as usize % directory_layout.len()
            };
            let relative_path = directory_layout[directory_index].join(file_name);
            let staged_path = source_dir.join(&relative_path);
            let staged_parent = staged_path.parent().with_context(|| {
                format!(
                    "staged path {} is unexpectedly missing a parent directory",
                    staged_path.display()
                )
            })?;
            fs::create_dir_all(staged_parent).with_context(|| {
                format!(
                    "failed to create staged directory {}",
                    staged_parent.display()
                )
            })?;
            let size_bytes = file_size_for_index(config, index);
            let seed = file_seed_for_index(index);
            let content_hash = write_random_file(&staged_path, size_bytes, seed)?;
            let store_path = normalized_store_path(&relative_path)?;

            specs.push(FileSpec {
                relative_path,
                store_path,
                size_bytes,
                content_hash,
            });
            total_bytes = total_bytes.saturating_add(size_bytes);

            if (index + 1) % 100 == 0 || index + 1 == config.file_count {
                eprintln!(
                    "[workload] staged {}/{} files in source tree across {} subdirs (logical {:.2} GiB)",
                    index + 1,
                    config.file_count,
                    config.subdir_count,
                    total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
        }

        Ok(specs)
    }

    fn copy_staged_workload_into_sync_root(
        file_specs: &[FileSpec],
        source_dir: &Path,
        sync_root: &Path,
    ) -> Result<()> {
        let mut total_bytes = 0usize;

        for (index, spec) in file_specs.iter().enumerate() {
            let staged_path = source_dir.join(&spec.relative_path);
            let target_path = sync_root.join(&spec.relative_path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create sync-root directory {}", parent.display())
                })?;
            }

            let copied = fs::copy(&staged_path, &target_path).with_context(|| {
                format!(
                    "failed to copy staged file {} into sync root {}",
                    staged_path.display(),
                    target_path.display()
                )
            })?;
            if copied as usize != spec.size_bytes {
                bail!(
                    "copy size mismatch for {}: expected={} copied={copied}",
                    target_path.display(),
                    spec.size_bytes
                );
            }

            total_bytes = total_bytes.saturating_add(spec.size_bytes);
            if (index + 1) % 100 == 0 || index + 1 == file_specs.len() {
                eprintln!(
                    "[workload] copied {}/{} staged files into CFAPI root (logical {:.2} GiB)",
                    index + 1,
                    file_specs.len(),
                    total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
        }

        Ok(())
    }

    #[tokio::test]
    #[ignore = "expensive local Windows CFAPI cluster workload"]
    async fn windows_cfapi_cluster_upload_and_replication_workload() -> Result<()> {
        let config = WorkloadConfig::from_env()?;
        let cluster_id = Uuid::new_v4().to_string();
        let http = Client::new();
        let sync_root = fresh_data_dir("windows-cfapi-cluster-sync-root");
        let source_dir = fresh_data_dir("windows-cfapi-cluster-source");

        fs::create_dir_all(&sync_root)
            .with_context(|| format!("failed to create sync root {}", sync_root.display()))?;

        let mut node_a = start_cluster_node(
            "127.0.0.1:19341",
            "cluster-a",
            "00000000-0000-0000-0000-00000000a341",
            &cluster_id,
            3,
        )
        .await?;
        let mut node_b = start_cluster_node(
            "127.0.0.1:19342",
            "cluster-b",
            "00000000-0000-0000-0000-00000000b342",
            &cluster_id,
            3,
        )
        .await?;
        let mut node_c = start_cluster_node(
            "127.0.0.1:19343",
            "cluster-c",
            "00000000-0000-0000-0000-00000000c343",
            &cluster_id,
            3,
        )
        .await?;

        let workload_result = async {
            eprintln!(
                "[cluster] starting workload: files={} min_bytes={} max_bytes={} average_bytes={} subdirs={} max_depth={}",
                config.file_count,
                config.min_bytes,
                config.max_bytes,
                config.average_bytes(),
                config.subdir_count,
                config.max_dir_depth
            );

            register_full_mesh(
                &http,
                &[&node_a, &node_b, &node_c],
            )
            .await?;

            for node in [&node_a, &node_b, &node_c] {
                wait_for_online_nodes(&http, &node.base_url, 3, 240).await?;
                eprintln!("[cluster] {} reports 3 online nodes", node.label);
            }

            let _adapter = start_cfapi_adapter_with_bootstrap(
                "ironmesh.systemtest.cluster.load",
                "Ironmesh Cluster Load Test",
                &sync_root,
                250,
                &node_a.bootstrap_file,
            )
            .await?;

            let file_specs = stage_workload(&config, &source_dir)?;
            copy_staged_workload_into_sync_root(&file_specs, &source_dir, &sync_root)?;
            let expected_paths = file_specs
                .iter()
                .map(|spec| spec.store_path.clone())
                .collect::<BTreeSet<_>>();

            eprintln!(
                "[cluster-a] waiting for {} uploaded files to appear on the ingress node",
                expected_paths.len()
            );
            wait_for_store_file_paths(&node_a.sdk, &expected_paths, node_a.label, UPLOAD_TIMEOUT)
                .await?;
            wait_for_local_subjects(
                &node_a.internal_http,
                &node_a.internal_base_url,
                &expected_paths,
                node_a.label,
                UPLOAD_TIMEOUT,
            )
            .await?;
            eprintln!("[cluster-a] upload convergence complete");

            eprintln!(
                "[cluster] driving replication to completion from {}",
                node_a.internal_base_url
            );
            drive_replication_to_completion(
                &node_a.internal_http,
                &node_a.internal_base_url,
                REPLICATION_TIMEOUT,
            )
            .await?;

            for node in [&node_a, &node_b, &node_c] {
                wait_for_store_file_paths(
                    &node.sdk,
                    &expected_paths,
                    node.label,
                    REPLICATION_TIMEOUT,
                )
                .await?;
                wait_for_local_subjects(
                    &node.internal_http,
                    &node.internal_base_url,
                    &expected_paths,
                    node.label,
                    REPLICATION_TIMEOUT,
                )
                .await?;
                eprintln!(
                    "[{}] replication convergence complete for {} files",
                    node.label,
                    expected_paths.len()
                );
            }

            let samples = select_sample_specs(&file_specs, config.sample_verify_count);
            for node in [&node_a, &node_b, &node_c] {
                verify_sample_content(&node.sdk, node.label, &samples).await?;
            }

            let under_replicated =
                current_under_replicated(&node_a.internal_http, &node_a.internal_base_url).await?;
            if under_replicated != 0 {
                bail!("replication plan still reports under_replicated={under_replicated}");
            }

            let total_bytes = file_specs
                .iter()
                .fold(0usize, |acc, spec| acc.saturating_add(spec.size_bytes));
            eprintln!(
                "[cluster] workload complete: files={} logical_gib={:.2} sampled_verifications={}",
                file_specs.len(),
                total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                samples.len()
            );

            Ok::<(), anyhow::Error>(())
        }
        .await;

        let _ = fs::remove_dir_all(&source_dir);
        let _ = fs::remove_dir_all(&sync_root);
        node_c.stop_and_cleanup().await;
        node_b.stop_and_cleanup().await;
        node_a.stop_and_cleanup().await;

        workload_result
    }
}
