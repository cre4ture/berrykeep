use crate::content_fingerprint::FingerprintingReader;
use crate::placeholder_metadata::{RemoteObjectResolver, ResolvedRemoteObject};
use crate::runtime::{
    HydrationProgress, HydrationRequest, HydrationResult, Hydrator, UploadReceipt, Uploader,
};
use anyhow::{Context, Result, anyhow};
use client_sdk::ironmesh_client::{DownloadProgress, DownloadRangeRequest, ObjectLookup};
use client_sdk::{
    ClientIdentityMaterial, IronMeshClient, build_http_client_from_pem,
    build_http_client_with_identity_from_pem, normalize_server_base_url,
};
use common::range_chunk_cache::{RANGE_CHUNK_CACHE_CHUNK_SIZE_BYTES, RangeChunkCache};
use reqwest::Url;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RangeChunkCacheKey {
    path: String,
    remote_version: String,
    chunk_index: u64,
}

impl RangeChunkCacheKey {
    fn new(path: &str, remote_version: &str, chunk_index: u64) -> Self {
        Self {
            path: path.to_string(),
            remote_version: remote_version.to_string(),
            chunk_index,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedRangeChunk {
    object_size_bytes: u64,
    payload: Vec<u8>,
}

#[derive(Clone)]
pub struct ServerNodeHydrator {
    sdk: IronMeshClient,
    download_stage_root: PathBuf,
    range_chunk_cache: Arc<Mutex<RangeChunkCache<RangeChunkCacheKey, CachedRangeChunk>>>,
}

impl ServerNodeHydrator {
    pub fn with_client(sdk: IronMeshClient, download_stage_root: PathBuf) -> Self {
        Self {
            sdk,
            download_stage_root,
            range_chunk_cache: Arc::new(Mutex::new(RangeChunkCache::default())),
        }
    }

    pub fn new(
        base_url: Url,
        client_identity: Option<ClientIdentityMaterial>,
        server_ca_pem: Option<&str>,
    ) -> Result<Self> {
        let sdk = match client_identity.as_ref() {
            Some(identity) => build_http_client_with_identity_from_pem(
                server_ca_pem,
                base_url.as_str(),
                identity,
            )?,
            None => build_http_client_from_pem(server_ca_pem, base_url.as_str())?,
        };
        Ok(Self::with_client(
            sdk,
            windows_download_stage_root(base_url.as_str())?,
        ))
    }

    fn read_cached_range_chunk(
        &self,
        path: &str,
        remote_version: &str,
        chunk_index: u64,
    ) -> Result<Option<Arc<CachedRangeChunk>>> {
        let key = RangeChunkCacheKey::new(path, remote_version, chunk_index);
        let mut cache = self
            .range_chunk_cache
            .lock()
            .map_err(|_| anyhow!("range chunk cache lock poisoned"))?;
        Ok(cache.get(&key))
    }

    fn cache_range_chunk(
        &self,
        path: &str,
        remote_version: &str,
        chunk_index: u64,
        chunk: CachedRangeChunk,
    ) -> Result<Arc<CachedRangeChunk>> {
        if chunk.payload.is_empty() {
            return Ok(Arc::new(chunk));
        }

        let key = RangeChunkCacheKey::new(path, remote_version, chunk_index);
        let mut cache = self
            .range_chunk_cache
            .lock()
            .map_err(|_| anyhow!("range chunk cache lock poisoned"))?;
        Ok(cache.insert(key, chunk))
    }

    fn download_range_chunk(
        &self,
        path: &str,
        chunk_start: u64,
        chunk_length: u64,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<CachedRangeChunk> {
        let mut downloaded = Vec::new();
        let mut on_progress = |_progress: DownloadProgress| {};
        let result = self
            .sdk
            .download_range_to_writer_with_progress_blocking(
                DownloadRangeRequest {
                    key: path,
                    snapshot: None,
                    version: None,
                    range: client_sdk::RequestedRange {
                        offset: chunk_start,
                        length: chunk_length,
                    },
                },
                &mut downloaded,
                &mut on_progress,
                should_cancel,
            )
            .with_context(|| {
                format!(
                    "failed to fetch ranged object chunk for path {path} chunk_offset={chunk_start}"
                )
            })?;

        Ok(CachedRangeChunk {
            object_size_bytes: result.object_size_bytes,
            payload: downloaded,
        })
    }
}

impl Hydrator for ServerNodeHydrator {
    fn hydrate(&self, path: &str, _remote_version: &str) -> Result<Vec<u8>> {
        tracing::info!("hydrating path {path} from server");
        let mut bytes = Vec::new();
        self.sdk
            .download_to_writer_resumable_staged(
                path,
                None,
                None,
                &mut bytes,
                &self.download_stage_root,
            )
            .with_context(|| format!("failed to fetch object for path {path}"))?;
        Ok(bytes)
    }

    fn hydrate_range_to_writer(
        &self,
        request: HydrationRequest<'_>,
        writer: &mut dyn Write,
        on_progress: &mut dyn FnMut(HydrationProgress),
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<HydrationResult> {
        tracing::info!(
            "hydrating range path {path} from server required_range={} transfer_range={}",
            request.required_range,
            request.transfer_range,
            path = request.path,
        );
        if request.transfer_range.length == 0 {
            return Ok(HydrationResult {
                object_size_bytes: 0,
                range: request.transfer_range,
                bytes_transferred: 0,
            });
        }

        let chunk_size = RANGE_CHUNK_CACHE_CHUNK_SIZE_BYTES as u64;
        let range_end_exclusive = request
            .transfer_range
            .offset
            .saturating_add(request.transfer_range.length);
        let first_chunk_index = request.transfer_range.offset / chunk_size;
        let last_chunk_index = range_end_exclusive.saturating_sub(1) / chunk_size;
        let mut bytes_transferred = 0_u64;
        let mut object_size_bytes = 0_u64;

        for chunk_index in first_chunk_index..=last_chunk_index {
            if should_cancel() {
                return Err(anyhow!("hydration canceled for {}", request.path));
            }

            let chunk_start = chunk_index.saturating_mul(chunk_size);
            let chunk = if let Some(chunk) =
                self.read_cached_range_chunk(request.path, request.remote_version, chunk_index)?
            {
                chunk
            } else {
                let downloaded = self.download_range_chunk(
                    request.path,
                    chunk_start,
                    chunk_size,
                    should_cancel,
                )?;
                self.cache_range_chunk(
                    request.path,
                    request.remote_version,
                    chunk_index,
                    downloaded,
                )?
            };

            object_size_bytes = object_size_bytes.max(chunk.object_size_bytes);
            let slice_start = request.transfer_range.offset.saturating_sub(chunk_start) as usize;
            let slice_end = range_end_exclusive
                .min(chunk_start.saturating_add(chunk.payload.len() as u64))
                .saturating_sub(chunk_start) as usize;
            if slice_start < slice_end {
                writer
                    .write_all(&chunk.payload[slice_start..slice_end])
                    .map_err(|err| {
                        anyhow!("failed to write hydrated bytes for {}: {err}", request.path)
                    })?;
                bytes_transferred =
                    bytes_transferred.saturating_add(slice_end.saturating_sub(slice_start) as u64);
                on_progress(HydrationProgress {
                    object_size_bytes,
                    range: request.transfer_range,
                    bytes_transferred,
                });
            }

            if chunk.payload.len() < RANGE_CHUNK_CACHE_CHUNK_SIZE_BYTES {
                break;
            }
        }

        writer
            .flush()
            .map_err(|err| anyhow!("failed to flush hydrated bytes for {}: {err}", request.path))?;

        if object_size_bytes == 0 {
            object_size_bytes = request
                .transfer_range
                .offset
                .saturating_add(bytes_transferred);
        }

        Ok(HydrationResult {
            object_size_bytes,
            range: request.transfer_range,
            bytes_transferred,
        })
    }
}

impl Uploader for ServerNodeHydrator {
    fn upload_reader(
        &self,
        path: &str,
        reader: &mut dyn std::io::Read,
        length: u64,
    ) -> Result<UploadReceipt> {
        self.upload_reader_for_object(path, None, None, reader, length)
    }

    fn upload_reader_for_object(
        &self,
        path: &str,
        object_id: Option<&str>,
        expected_revision: Option<&str>,
        reader: &mut dyn std::io::Read,
        length: u64,
    ) -> Result<UploadReceipt> {
        let mut fingerprinting_reader = FingerprintingReader::new(reader, length);
        let mutation = self
            .sdk
            .put_reader_with_identity_blocking(
                path.to_string(),
                object_id,
                expected_revision,
                &mut fingerprinting_reader,
                length,
            )
            .with_context(|| {
                format!(
                    "failed to upload object for path {path} object_id={} expected_revision={}",
                    object_id.unwrap_or("<new>"),
                    expected_revision.unwrap_or("<none>")
                )
            })?;
        let in_sync_content_fingerprint = fingerprinting_reader
            .finish()
            .with_context(|| format!("failed to finalize content fingerprint for {path}"))?;

        Ok(UploadReceipt {
            object_id: Some(mutation.object_id),
            remote_version: Some(mutation.revision),
            in_sync_content_fingerprint: Some(in_sync_content_fingerprint),
        })
    }

    fn delete_object(&self, object_id: &str, expected_revision: &str) -> Result<()> {
        self.sdk
            .delete_object_by_id_blocking(object_id, Some(expected_revision))
            .with_context(|| {
                format!(
                    "failed to delete remote object object_id={object_id} expected_revision={expected_revision}"
                )
            })?;
        Ok(())
    }

    fn rename_object(
        &self,
        object_id: &str,
        expected_revision: &str,
        to_path: &str,
    ) -> Result<bool> {
        self.sdk
            .rename_object_by_id_blocking(object_id, to_path, false, Some(expected_revision))
            .with_context(|| {
                format!(
                    "failed to rename remote object object_id={object_id} expected_revision={expected_revision} to {to_path}"
                )
            })?;
        Ok(true)
    }
}

impl RemoteObjectResolver for ServerNodeHydrator {
    fn resolve_object(&self, object_id: &str) -> Result<Option<ResolvedRemoteObject>> {
        self.sdk
            .lookup_object_by_id_blocking(object_id)
            .map(|resolved| resolved.and_then(active_resolved_remote_object))
    }
}

fn active_resolved_remote_object(resolved: ObjectLookup) -> Option<ResolvedRemoteObject> {
    // The identity endpoint deliberately retains tombstones so a client can
    // inspect their final revision.  CFAPI reconciliation needs the active
    // namespace, though: a tombstone confirms that this object no longer has
    // a remotely live path.
    (resolved.entry_type != "tombstone").then_some(ResolvedRemoteObject {
        object_id: resolved.object_id,
        path: resolved.path,
        revision: resolved.revision,
    })
}

pub fn normalize_base_url(input: &str) -> Result<Url> {
    normalize_server_base_url(input)
}

const WINDOWS_LOCAL_STATE_ROOT_DIR: &str = "Ironmesh";
const WINDOWS_DOWNLOAD_STAGE_SUBDIR: &str = "cfapi-downloads";

pub fn windows_download_stage_root(scope: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = windows_download_stage_base_root(base).join(download_scope_label(scope));
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create download stage root {}", path.display()))?;
    Ok(path)
}

pub fn windows_download_stage_root_for_sync_root(sync_root: &Path) -> Result<PathBuf> {
    windows_download_stage_root(&sync_root.to_string_lossy())
}

fn download_scope_label(scope: &str) -> String {
    blake3::hash(scope.as_bytes()).to_hex().to_string()
}

fn windows_download_stage_base_root(base: PathBuf) -> PathBuf {
    base.join(WINDOWS_LOCAL_STATE_ROOT_DIR)
        .join(WINDOWS_DOWNLOAD_STAGE_SUBDIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    fn capture_single_http_request() -> (String, std::thread::JoinHandle<()>, mpsc::Receiver<String>)
    {
        capture_single_http_request_with_response(
            b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
    }

    fn capture_single_http_request_with_response(
        response: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<()>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should expose its address");
        let (request_tx, request_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test request should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should be configurable");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4_096];
            loop {
                let read = stream.read(&mut chunk).expect("request should be readable");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let body_start = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= body_start + content_length {
                        break;
                    }
                }
            }
            request_tx
                .send(String::from_utf8_lossy(&request).into_owned())
                .expect("captured request should be delivered");
            stream
                .write_all(&response)
                .expect("test response should be writable");
        });
        (format!("http://{address}"), handle, request_rx)
    }

    #[test]
    fn base_url_normalization_adds_scheme_and_trailing_slash() {
        let url = normalize_base_url("127.0.0.1:18080").expect("url should be valid");
        assert_eq!(url.as_str(), "http://127.0.0.1:18080/");
    }

    #[test]
    fn windows_download_stage_root_uses_ironmesh_localappdata_root() {
        let base = PathBuf::from("C:/Users/Example/AppData/Local");
        assert_eq!(
            windows_download_stage_base_root(base.clone()),
            base.join("Ironmesh").join("cfapi-downloads")
        );
    }

    #[test]
    fn object_resolver_interprets_an_identity_tombstone_as_absent() {
        let tombstone = ObjectLookup {
            object_id: "object-deleted".to_string(),
            path: "docs/deleted.txt".to_string(),
            revision: Some("revision-tombstone".to_string()),
            entry_type: "tombstone".to_string(),
        };

        assert!(active_resolved_remote_object(tombstone).is_none());
    }

    #[test]
    fn windows_delete_request_uses_object_id_and_revision_precondition() {
        let (base_url, server, request_rx) = capture_single_http_request();
        let client = IronMeshClient::from_direct_base_url(base_url);
        let hydrator = ServerNodeHydrator::with_client(
            client,
            std::env::temp_dir().join(format!("ironmesh-delete-request-{}", uuid::Uuid::new_v4())),
        );

        Uploader::delete_object(&hydrator, "obj-stale", "revision-7")
            .expect("conditional delete request should be accepted by the test server");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("delete request should be captured");
        server.join().expect("test server should stop cleanly");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("DELETE /api/v1/objects/obj-stale?"));
        assert!(
            request_line.contains("expected_revision=revision-7"),
            "object delete must carry its observed revision: {request_line}"
        );
    }

    #[test]
    fn windows_rename_request_uses_object_id_and_revision_precondition() {
        let (base_url, server, request_rx) = capture_single_http_request_with_response(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let hydrator = ServerNodeHydrator::with_client(
            IronMeshClient::from_direct_base_url(base_url),
            std::env::temp_dir().join(format!("ironmesh-rename-request-{}", uuid::Uuid::new_v4())),
        );

        assert!(
            Uploader::rename_object(&hydrator, "obj-report", "revision-11", "archive/report.txt",)
                .expect("conditional rename should be accepted by the test server")
        );

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("rename request should be captured");
        server.join().expect("test server should stop cleanly");
        assert!(
            request
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("POST /api/v1/objects/obj-report/rename ")
        );
        assert!(request.contains(r#""to_path":"archive/report.txt""#));
        assert!(request.contains(r#""expected_revision":"revision-11""#));
    }

    #[test]
    fn windows_modify_request_uses_object_id_and_expected_revision() {
        let body =
            br#"{"object_id":"obj-photo","path":"photos/photo.jpg","revision":"revision-8"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).expect("response body is utf8")
        )
        .into_bytes();
        let (base_url, server, request_rx) = capture_single_http_request_with_response(response);
        let hydrator = ServerNodeHydrator::with_client(
            IronMeshClient::from_direct_base_url(base_url),
            std::env::temp_dir().join(format!("ironmesh-upload-request-{}", uuid::Uuid::new_v4())),
        );
        let payload = b"new content";
        let mut reader = std::io::Cursor::new(payload);

        let receipt = Uploader::upload_reader_for_object(
            &hydrator,
            "photos/photo.jpg",
            Some("obj-photo"),
            Some("revision-7"),
            &mut reader,
            payload.len() as u64,
        )
        .expect("object-id upload should succeed");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("upload request should be captured");
        server.join().expect("test server should stop cleanly");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("PUT /api/v1/objects/obj-photo?"));
        assert!(request_line.contains("expected_revision=revision-7"));
        assert_eq!(receipt.object_id.as_deref(), Some("obj-photo"));
        assert_eq!(receipt.remote_version.as_deref(), Some("revision-8"));
    }

    #[test]
    fn stale_expected_revision_is_reported_as_conflict_without_fallback() {
        let (base_url, server, request_rx) = capture_single_http_request_with_response(
            b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let hydrator = ServerNodeHydrator::with_client(
            IronMeshClient::from_direct_base_url(base_url),
            std::env::temp_dir().join(format!("ironmesh-upload-conflict-{}", uuid::Uuid::new_v4())),
        );
        let payload = b"local content";
        let mut reader = std::io::Cursor::new(payload);

        let error = Uploader::upload_reader_for_object(
            &hydrator,
            "photos/photo.jpg",
            Some("obj-photo"),
            Some("stale-revision"),
            &mut reader,
            payload.len() as u64,
        )
        .expect_err("stale CAS must not fall back to a path mutation");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("upload request should be captured");
        server.join().expect("test server should stop cleanly");
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("PUT /api/v1/objects/obj-photo?"));
        assert!(request_line.contains("expected_revision=stale-revision"));
        assert!(format!("{error:#}").contains("409 Conflict"));
        assert!(crate::runtime::is_remote_mutation_conflict(&error));
    }
}
