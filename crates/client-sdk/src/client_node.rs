use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use common::{CacheEntry, StorageObjectMeta};
use tokio::sync::RwLock;

use crate::ironmesh_client::{
    IronMeshClient, ObjectHeadInfo, SnapshotRestoreResponse, StoreIndexResponse, StoreIndexView,
    UploadResult, VersionGraphSummary,
};

#[derive(Clone, Copy)]
struct ClientContentCacheLimits {
    capacity_bytes: usize,
    max_entry_bytes: usize,
    capacity_entries: usize,
}

struct ClientContentCache {
    entries: HashMap<String, Bytes>,
    recency: VecDeque<String>,
    size_bytes: usize,
    limits: Option<ClientContentCacheLimits>,
}

impl ClientContentCache {
    fn unbounded() -> Self {
        Self {
            entries: HashMap::new(),
            recency: VecDeque::new(),
            size_bytes: 0,
            limits: None,
        }
    }

    fn bounded(capacity_bytes: usize, max_entry_bytes: usize, capacity_entries: usize) -> Self {
        Self {
            limits: Some(ClientContentCacheLimits {
                capacity_bytes,
                max_entry_bytes: max_entry_bytes.min(capacity_bytes),
                capacity_entries,
            }),
            ..Self::unbounded()
        }
    }

    fn get(&mut self, key: &str) -> Option<Bytes> {
        let payload = self.entries.get(key)?.clone();
        if self.limits.is_some() {
            self.recency.retain(|cached_key| cached_key != key);
            self.recency.push_back(key.to_string());
        }
        Some(payload)
    }

    fn insert(&mut self, key: String, payload: Bytes) {
        self.remove(&key);
        let Some(limits) = self.limits else {
            self.size_bytes = self.size_bytes.saturating_add(payload.len());
            self.entries.insert(key, payload);
            return;
        };
        if limits.capacity_entries == 0 || payload.len() > limits.max_entry_bytes {
            return;
        }

        while self.entries.len() >= limits.capacity_entries
            || self.size_bytes.saturating_add(payload.len()) > limits.capacity_bytes
        {
            let Some(evicted_key) = self.recency.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&evicted_key) {
                self.size_bytes = self.size_bytes.saturating_sub(evicted.len());
            }
        }

        self.size_bytes = self.size_bytes.saturating_add(payload.len());
        self.recency.push_back(key.clone());
        self.entries.insert(key, payload);
    }

    fn remove(&mut self, key: &str) -> Option<Bytes> {
        let removed = self.entries.remove(key)?;
        self.size_bytes = self.size_bytes.saturating_sub(removed.len());
        self.recency.retain(|cached_key| cached_key != key);
        Some(removed)
    }

    fn remove_where(&mut self, mut predicate: impl FnMut(&str) -> bool) {
        let keys = self
            .entries
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove(&key);
        }
    }

    fn cache_entries(&self) -> Vec<CacheEntry> {
        self.entries
            .iter()
            .map(|(key, value)| CacheEntry {
                key: key.clone(),
                size_bytes: value.len(),
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct ClientNode {
    client: IronMeshClient,
    cache: Arc<RwLock<ClientContentCache>>,
}

impl ClientNode {
    pub fn from_direct_base_url(server_base_url: impl Into<String>) -> Self {
        Self::with_client(IronMeshClient::from_direct_base_url(server_base_url))
    }

    pub fn from_direct_http_client(
        server_base_url: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self::with_client(IronMeshClient::from_direct_http_client(
            server_base_url,
            http,
        ))
    }

    pub fn with_client(client: IronMeshClient) -> Self {
        Self {
            client,
            cache: Arc::new(RwLock::new(ClientContentCache::unbounded())),
        }
    }

    /// Creates a client node with a byte-weighted LRU content cache. Payloads
    /// larger than `max_entry_bytes` are served but never retained.
    pub fn with_client_cache_limits(
        client: IronMeshClient,
        capacity_bytes: usize,
        max_entry_bytes: usize,
        capacity_entries: usize,
    ) -> Self {
        Self {
            client,
            cache: Arc::new(RwLock::new(ClientContentCache::bounded(
                capacity_bytes,
                max_entry_bytes,
                capacity_entries,
            ))),
        }
    }

    /// Returns a differently named client view while retaining this node's
    /// in-process content cache.
    pub fn with_connection_name(mut self, connection_name: impl Into<String>) -> Self {
        self.client = self.client.with_connection_name(connection_name);
        self
    }

    pub async fn put(&self, key: impl Into<String>, data: Bytes) -> Result<StorageObjectMeta> {
        self.put_with_expected_revision(key, data, None).await
    }

    pub async fn put_with_expected_revision(
        &self,
        key: impl Into<String>,
        data: Bytes,
        expected_revision: Option<&str>,
    ) -> Result<StorageObjectMeta> {
        let key = key.into();
        let meta = self
            .client
            .put_with_expected_revision(key.clone(), data.clone(), expected_revision)
            .await?;

        self.cache.write().await.insert(key.clone(), data.clone());
        Ok(meta)
    }

    pub async fn put_large_aware(
        &self,
        key: impl Into<String>,
        data: Bytes,
    ) -> Result<UploadResult> {
        let key = key.into();
        let report = self
            .client
            .put_large_aware(key.clone(), data.clone())
            .await?;
        self.cache.write().await.insert(key, data);
        Ok(report)
    }

    pub async fn get(&self, key: impl AsRef<str>) -> Result<Bytes> {
        let key = key.as_ref();
        let payload = self.client.get(key).await?;

        self.cache
            .write()
            .await
            .insert(key.to_string(), payload.clone());

        Ok(payload)
    }

    pub async fn get_cached_or_fetch(&self, key: impl AsRef<str>) -> Result<Bytes> {
        let key = key.as_ref();

        if let Some(entry) = self.cache.write().await.get(key) {
            return Ok(entry);
        }

        self.get(key).await
    }

    pub async fn get_with_selector(
        &self,
        key: impl AsRef<str>,
        snapshot: Option<&str>,
        version: Option<&str>,
    ) -> Result<Bytes> {
        let key = key.as_ref();
        let payload = self
            .client
            .get_with_selector(key, snapshot, version)
            .await?;

        if snapshot.is_none() && version.is_none() {
            self.cache
                .write()
                .await
                .insert(key.to_string(), payload.clone());
        }

        Ok(payload)
    }

    pub async fn get_history_source_version(
        &self,
        key: impl AsRef<str>,
        source_object_id: &str,
        version: &str,
    ) -> Result<Bytes> {
        self.client
            .get_history_source_version(key, source_object_id, version)
            .await
    }

    pub async fn rename_path(
        &self,
        from_path: impl Into<String>,
        to_path: impl Into<String>,
        overwrite: bool,
    ) -> Result<()> {
        self.rename_path_with_expected_revision(from_path, to_path, overwrite, None)
            .await
    }

    pub async fn rename_path_with_expected_revision(
        &self,
        from_path: impl Into<String>,
        to_path: impl Into<String>,
        overwrite: bool,
        expected_revision: Option<&str>,
    ) -> Result<()> {
        let from_path = from_path.into();
        let to_path = to_path.into();
        self.client
            .rename_path_with_expected_revision(
                from_path.clone(),
                to_path.clone(),
                overwrite,
                expected_revision,
            )
            .await?;

        let mut cache = self.cache.write().await;
        if let Some(payload) = cache.remove(&from_path) {
            cache.insert(to_path, payload);
        }
        Ok(())
    }

    pub async fn copy_path(
        &self,
        from_path: impl Into<String>,
        to_path: impl Into<String>,
        overwrite: bool,
    ) -> Result<()> {
        let from_path = from_path.into();
        let to_path = to_path.into();
        self.client
            .copy_path(from_path.clone(), to_path.clone(), overwrite)
            .await?;

        let mut cache = self.cache.write().await;
        if let Some(payload) = cache.get(&from_path) {
            cache.insert(to_path, payload);
        }
        Ok(())
    }

    pub async fn restore_path_from_snapshot(
        &self,
        snapshot: impl Into<String>,
        from_path: impl Into<String>,
        to_path: impl Into<String>,
        recursive: bool,
        overwrite: bool,
    ) -> Result<SnapshotRestoreResponse> {
        let snapshot = snapshot.into();
        let from_path = from_path.into();
        let to_path = to_path.into();
        let response = self
            .client
            .restore_path_from_snapshot(
                snapshot,
                from_path.clone(),
                to_path.clone(),
                recursive,
                overwrite,
            )
            .await?;

        let mut cache = self.cache.write().await;
        if recursive {
            cache.remove_where(|key| key == to_path || key.starts_with(&to_path));
        } else {
            cache.remove(&to_path);
        }

        Ok(response)
    }

    pub async fn delete_path(&self, key: impl Into<String>) -> Result<()> {
        self.delete_path_with_expected_revision(key, None).await
    }

    pub async fn delete_path_with_expected_revision(
        &self,
        key: impl Into<String>,
        expected_revision: Option<&str>,
    ) -> Result<()> {
        let key = key.into();
        self.client
            .delete_path_with_expected_revision(&key, expected_revision)
            .await?;
        self.cache.write().await.remove(&key);
        Ok(())
    }

    pub async fn store_index(
        &self,
        prefix: Option<&str>,
        depth: usize,
        snapshot: Option<&str>,
    ) -> Result<StoreIndexResponse> {
        self.client
            .store_index_with_view(prefix, depth, snapshot, Some(StoreIndexView::Tree))
            .await
    }

    pub async fn list_versions(&self, key: impl AsRef<str>) -> Result<Option<VersionGraphSummary>> {
        self.client.list_versions(key).await
    }

    pub async fn restore_version_path(
        &self,
        key: impl Into<String>,
        version_id: impl Into<String>,
        to_path: impl Into<String>,
        overwrite: bool,
    ) -> Result<()> {
        let key = key.into();
        let version_id = version_id.into();
        let to_path = to_path.into();
        self.client
            .restore_version_path(key, version_id, to_path.clone(), overwrite)
            .await?;
        self.cache.write().await.remove(&to_path);
        Ok(())
    }

    pub async fn head_object(
        &self,
        key: impl AsRef<str>,
        snapshot: Option<&str>,
        version: Option<&str>,
    ) -> Result<ObjectHeadInfo> {
        self.client.head_object(key, snapshot, version).await
    }

    pub fn put_large_aware_reader(
        &self,
        key: impl Into<String>,
        reader: &mut dyn std::io::Read,
        length: u64,
    ) -> Result<UploadResult> {
        let key = key.into();
        let report = self
            .client
            .put_large_aware_reader(key.clone(), reader, length)?;
        self.cache.blocking_write().remove(&key);
        Ok(report)
    }

    pub fn get_with_selector_writer(
        &self,
        key: impl AsRef<str>,
        snapshot: Option<&str>,
        version: Option<&str>,
        writer: &mut dyn std::io::Write,
    ) -> Result<()> {
        self.client
            .get_with_selector_writer(key, snapshot, version, writer)
    }

    pub fn download_to_writer_resumable_staged(
        &self,
        key: impl AsRef<str>,
        snapshot: Option<&str>,
        version: Option<&str>,
        writer: &mut dyn std::io::Write,
        staging_root: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        self.client.download_to_writer_resumable_staged(
            key,
            snapshot,
            version,
            writer,
            staging_root,
        )
    }

    pub async fn cache_entries(&self) -> Vec<CacheEntry> {
        self.cache.read().await.cache_entries()
    }

    pub async fn remove_cached(&self, key: impl AsRef<str>) -> Result<()> {
        let key = key.as_ref();
        let removed = self.cache.write().await.remove(key);

        if removed.is_none() {
            return Err(anyhow!("cache key not present: {key}"));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn named_client_view_retains_shared_content_cache() {
        let node = ClientNode::from_direct_base_url("http://127.0.0.1:1");
        node.cache
            .write()
            .await
            .insert("cached.txt".to_string(), Bytes::from_static(b"cached"));

        let named = node.clone().with_connection_name("mobile test");

        assert_eq!(
            named.cache.write().await.get("cached.txt"),
            Some(Bytes::from_static(b"cached"))
        );
    }

    #[tokio::test]
    async fn default_content_cache_preserves_unbounded_large_entries() {
        let node = ClientNode::from_direct_base_url("http://127.0.0.1:1");
        let payload = Bytes::from(vec![7; 9 * 1024 * 1024]);
        let mut cache = node.cache.write().await;

        cache.insert("large.bin".to_string(), payload.clone());

        assert_eq!(cache.get("large.bin"), Some(payload));
        assert!(cache.limits.is_none());
        assert!(cache.recency.is_empty());
    }

    #[tokio::test]
    async fn content_cache_evicts_by_recency_and_rejects_oversized_entries() {
        let node = ClientNode::with_client_cache_limits(
            IronMeshClient::from_direct_base_url("http://127.0.0.1:1"),
            6,
            4,
            2,
        );
        let mut cache = node.cache.write().await;
        cache.insert("a".to_string(), Bytes::from_static(b"aaa"));
        cache.insert("b".to_string(), Bytes::from_static(b"bbb"));
        assert_eq!(cache.get("a"), Some(Bytes::from_static(b"aaa")));

        cache.insert("c".to_string(), Bytes::from_static(b"ccc"));
        cache.insert("large".to_string(), Bytes::from_static(b"12345"));

        assert_eq!(cache.size_bytes, 6);
        assert_eq!(cache.entries.len(), 2);
        assert!(!cache.entries.contains_key("b"));
        assert!(!cache.entries.contains_key("large"));
        assert!(cache.entries.contains_key("a"));
        assert!(cache.entries.contains_key("c"));
    }
}
