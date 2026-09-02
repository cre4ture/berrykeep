import { fetchJson, isHttpErrorStatus } from "../shared/http";
import type { GalleryMapConfigurationResponse } from "../shared/map-config";
import {
  galleryMapClusterCellSizeParameter,
  galleryMapClusterZoomParameters
} from "../shared/store-index";
import type {
  GalleryMapClusterEntriesResponse,
  GalleryMapClustersRequest,
  GalleryMapClustersResponse,
  StoreIndexMedia
} from "../shared/store-index";
import type {
  StoreIndexDeltaResponse,
  ClientConnectionRouteSnapshot,
  ClientCacheContextResponse,
  ClientDeviceIdentityView,
  ClientDiagnosticLogExport,
  ClientLatencyTestResponse,
  ClientRendezvousView,
  ClientUiPingResponse,
  ClientWebService,
  ClientWebServiceLaunchResponse,
  ClientWebServiceNodeListResponse,
  ClientWebServiceNodeResponse,
  JsonObject,
  LogsResponse,
  SnapshotSummary,
  StoreGetResponse,
  StoreHistoryRestoreEntry,
  StoreHistoryRestoreResponse,
  StoreHistoryResponse,
  StoreListRequestOptions,
  StoreListResponse,
  StoreListView,
  StorePutResponse,
  StoreUploadSessionChunkResponse,
  StoreUploadSessionCompleteResponse,
  StoreUploadSessionStartResponse,
  VersionGraphResponse
} from "./types";

export type BinaryDownloadResult = {
  blob: Blob;
  filename: string;
  contentType: string;
};

export type BinaryUploadProgress = {
  uploadedBytes: number;
  totalBytes: number;
  uploadedChunks: number;
  totalChunks: number;
  percent: number;
  phase: "starting" | "uploading" | "finalizing" | "complete";
};

export type BinaryUploadOptions = {
  signal?: AbortSignal;
};

export type ClientWebServiceRequestOptions = {
  signal?: AbortSignal;
};

const API_V1_PREFIX = "/api/v1";
const DIAGNOSTIC_CONTEXT_HEADER = "x-ironmesh-diagnostic-context";
type GalleryMapEndpoint = "clusters" | "clusterEntries";

const galleryMapEndpointPaths = {
  clusters: {
    canonical: "/gallery/map/clusters",
    legacy: "/store/map/clusters"
  },
  clusterEntries: {
    canonical: "/gallery/map/cluster-entries",
    legacy: "/store/map/cluster-entries"
  }
} as const;

const galleryMapEndpointRoutes = new Map<GalleryMapEndpoint, "canonical" | "legacy">();

export type ClientDiagnosticRequestOptions = {
  diagnosticContext?: string;
};

function apiV1(path: string): string {
  return `${API_V1_PREFIX}${path}`;
}

async function fetchGalleryMapJson<T>(
  endpoint: GalleryMapEndpoint,
  query: URLSearchParams
): Promise<T> {
  const paths = galleryMapEndpointPaths[endpoint];
  let route = galleryMapEndpointRoutes.get(endpoint) ?? "canonical";
  try {
    return await fetchJson<T>(`${apiV1(paths[route])}?${query.toString()}`);
  } catch (error) {
    if (route !== "canonical" || !isHttpErrorStatus(error, 404)) {
      throw error;
    }
    route = "legacy";
    galleryMapEndpointRoutes.set(endpoint, route);
    return fetchJson<T>(`${apiV1(paths[route])}?${query.toString()}`);
  }
}

export async function getClientPing(
  options?: ClientDiagnosticRequestOptions
): Promise<ClientUiPingResponse> {
  return fetchJson<ClientUiPingResponse>(apiV1("/ping"), diagnosticRequestInit(options));
}

export async function getClientCacheContext(): Promise<ClientCacheContextResponse> {
  return fetchJson<ClientCacheContextResponse>(apiV1("/cache-context"), {
    cache: "no-store",
    credentials: "same-origin"
  });
}

export async function getClientDeviceIdentity(
  options?: ClientDiagnosticRequestOptions
): Promise<ClientDeviceIdentityView> {
  const identity = await fetchJson<ClientDeviceIdentityView | null>(apiV1("/device-identity"), {
    cache: "no-store",
    credentials: "same-origin",
    ...diagnosticRequestInit(options)
  });
  if (identity === null) {
    throw new Error("Device identity response did not contain JSON");
  }
  return identity;
}

export async function getClientGalleryMapConfiguration(): Promise<GalleryMapConfigurationResponse> {
  return fetchJson<GalleryMapConfigurationResponse>(apiV1("/maps/config"), {
    cache: "no-store"
  });
}

export async function getClientHealth(options?: ClientDiagnosticRequestOptions): Promise<JsonObject> {
  return fetchJson<JsonObject>(apiV1("/health"), diagnosticRequestInit(options));
}

export async function getClientClusterStatus(options?: ClientDiagnosticRequestOptions): Promise<JsonObject> {
  return fetchJson<JsonObject>(apiV1("/cluster/status"), diagnosticRequestInit(options));
}

export async function getClientConnectionRoutes(): Promise<ClientConnectionRouteSnapshot> {
  return fetchJson<ClientConnectionRouteSnapshot>(apiV1("/connection-routes"));
}

export async function listClientWebServices(
  options?: ClientWebServiceRequestOptions
): Promise<ClientWebService[]> {
  return fetchJson<ClientWebService[]>(apiV1("/web-services"), {
    credentials: "same-origin",
    cache: "no-store",
    signal: options?.signal
  });
}

export async function listClientWebServiceNodes(
  options?: ClientWebServiceRequestOptions
): Promise<ClientWebServiceNodeListResponse> {
  return fetchJson<ClientWebServiceNodeListResponse>(apiV1("/web-services/nodes"), {
    credentials: "same-origin",
    cache: "no-store",
    signal: options?.signal
  });
}

export async function listClientWebServicesOnNode(
  nodeId: string,
  options?: ClientWebServiceRequestOptions
): Promise<ClientWebServiceNodeResponse> {
  return fetchJson<ClientWebServiceNodeResponse>(
    apiV1(`/web-services/nodes/${encodeURIComponent(nodeId)}`),
    {
      credentials: "same-origin",
      cache: "no-store",
      signal: options?.signal
    }
  );
}

export async function launchClientWebService(
  nodeId: string,
  serviceId: string
): Promise<ClientWebServiceLaunchResponse> {
  return fetchJson<ClientWebServiceLaunchResponse>(
    apiV1(`/web-services/${encodeURIComponent(nodeId)}/${encodeURIComponent(serviceId)}/launch`),
    {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store"
    }
  );
}

export async function refreshClientConnectionRoutes(): Promise<ClientConnectionRouteSnapshot> {
  return fetchJson<ClientConnectionRouteSnapshot>(apiV1("/connection-routes/refresh"), {
    method: "POST"
  });
}

export async function getClientRecentLogs(limit = 200): Promise<LogsResponse> {
  const resolvedLimit = Number.isFinite(limit) ? Math.max(1, Math.min(1000, Math.trunc(limit))) : 200;
  return fetchJson<LogsResponse>(`${apiV1("/logs")}?limit=${resolvedLimit}`, {
    credentials: "same-origin",
    cache: "no-store"
  });
}

export async function exportClientDiagnosticLogs(windowSecs = 180): Promise<ClientDiagnosticLogExport> {
  const resolvedWindowSecs = Number.isFinite(windowSecs)
    ? Math.max(1, Math.min(3600, Math.trunc(windowSecs)))
    : 180;
  return fetchJson<ClientDiagnosticLogExport>(
    `${apiV1("/diagnostics/log-export")}?window_secs=${resolvedWindowSecs}`,
    {
      credentials: "same-origin",
      cache: "no-store"
    }
  );
}

export async function getClientRendezvous(
  options?: ClientDiagnosticRequestOptions
): Promise<ClientRendezvousView> {
  return fetchJson<ClientRendezvousView>(apiV1("/rendezvous"), diagnosticRequestInit(options));
}

export async function refreshClientRendezvous(): Promise<ClientRendezvousView> {
  return fetchJson<ClientRendezvousView>(apiV1("/rendezvous/refresh"), {
    method: "POST"
  });
}

function diagnosticRequestInit(options?: ClientDiagnosticRequestOptions): RequestInit | undefined {
  const diagnosticContext = options?.diagnosticContext?.trim();
  if (!diagnosticContext) {
    return undefined;
  }

  return {
    headers: {
      [DIAGNOSTIC_CONTEXT_HEADER]: diagnosticContext
    }
  };
}

export async function runClientLatencyTest(request?: {
  sample_count?: number;
  warmup_count?: number;
  response_bytes?: number;
  server_delay_ms?: number;
  pause_between_samples_ms?: number;
}): Promise<ClientLatencyTestResponse> {
  return fetchJson<ClientLatencyTestResponse>(apiV1("/latency-test"), {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify(request ?? {})
  });
}

export async function updateClientRendezvous(request: {
  rendezvous_urls: string[];
}): Promise<ClientRendezvousView> {
  return fetchJson<ClientRendezvousView>(apiV1("/rendezvous"), {
    method: "PUT",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify(request)
  });
}

export async function getClientClusterNodes(): Promise<unknown[]> {
  return fetchJson<unknown[]>(apiV1("/cluster/nodes"));
}

export async function getClientReplicationPlan(): Promise<JsonObject> {
  return fetchJson<JsonObject>(apiV1("/cluster/replication/plan"));
}

export async function listSnapshots(): Promise<SnapshotSummary[]> {
  return fetchJson<SnapshotSummary[]>(apiV1("/snapshots"));
}

export async function listStoreEntries(
  prefix?: string,
  depth = 1,
  snapshot?: string | null,
  options: StoreListRequestOptions = {}
): Promise<StoreListResponse> {
  const view: StoreListView = options.view ?? "tree";
  const query = new URLSearchParams({
    depth: String(Math.max(1, depth))
  });
  if (prefix?.trim()) {
    query.set("prefix", prefix.trim());
  }
  if (snapshot?.trim()) {
    query.set("snapshot", snapshot.trim());
  }
  query.set("view", view);
  if (typeof options.offset === "number" && Number.isFinite(options.offset) && options.offset >= 0) {
    query.set("offset", String(Math.floor(options.offset)));
  }
  if (typeof options.limit === "number" && Number.isFinite(options.limit) && options.limit > 0) {
    query.set("limit", String(Math.max(1, Math.floor(options.limit))));
  }
  if (options.sort) {
    query.set("sort", options.sort);
  }
  if (options.mediaFilter) {
    query.set("media_filter", options.mediaFilter);
  }
  if (options.viewport) {
    query.set("south", String(options.viewport.south));
    query.set("west", String(options.viewport.west));
    query.set("north", String(options.viewport.north));
    query.set("east", String(options.viewport.east));
  }
  appendLabelFilter(query, "require_labels", options.requireLabels);
  appendLabelFilter(query, "exclude_labels", options.excludeLabels);
  return fetchJson<StoreListResponse>(`${apiV1("/store/list")}?${query.toString()}`);
}

export async function listStoreHistoryEntries(
  prefix?: string,
  depth = 1
): Promise<StoreHistoryResponse> {
  const query = new URLSearchParams({
    depth: String(Math.max(1, Math.floor(depth)))
  });
  if (prefix?.trim()) {
    query.set("prefix", prefix.trim());
  }
  return fetchJson<StoreHistoryResponse>(`${apiV1("/store/history")}?${query.toString()}`);
}

export async function restoreStoreHistoryEntries(
  entries: StoreHistoryRestoreEntry[]
): Promise<StoreHistoryRestoreResponse> {
  return fetchJson<StoreHistoryRestoreResponse>(apiV1("/store/history/restore"), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ entries })
  });
}

export async function setStoreMediaLabels(path: string, labels: string[]): Promise<void> {
  await fetchJson<unknown>(apiV1("/store/labels"), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ path, labels })
  });
}

export async function getGalleryMapClusters(
  request: GalleryMapClustersRequest
): Promise<GalleryMapClustersResponse> {
  const query = galleryMapClusterQuery(request);
  return fetchGalleryMapJson<GalleryMapClustersResponse>("clusters", query);
}

export async function getGalleryMapClusterEntries(
  queryToken: string,
  clusterId: string,
  offset = 0,
  limit = 100
): Promise<GalleryMapClusterEntriesResponse> {
  const query = new URLSearchParams({
    query_token: queryToken,
    cluster_id: clusterId,
    offset: String(Math.max(0, Math.floor(offset))),
    limit: String(Math.max(1, Math.floor(limit)))
  });
  return fetchGalleryMapJson<GalleryMapClusterEntriesResponse>("clusterEntries", query);
}

function galleryMapClusterQuery(request: GalleryMapClustersRequest): URLSearchParams {
  const { zoom, zoomPrecise } = galleryMapClusterZoomParameters(request.zoom);
  const cellSizePx = galleryMapClusterCellSizeParameter(request.clusterCellSizePx);
  const query = new URLSearchParams({
    depth: String(Math.max(1, Math.floor(request.depth))),
    media_filter: request.mediaFilter,
    south: String(request.viewport.south),
    west: String(request.viewport.west),
    north: String(request.viewport.north),
    east: String(request.viewport.east),
    zoom: String(zoom),
    zoom_precise: String(zoomPrecise)
  });
  if (request.prefix?.trim()) {
    query.set("prefix", request.prefix.trim());
  }
  if (request.resolutionViewport) {
    query.set("resolution_south", String(request.resolutionViewport.south));
    query.set("resolution_west", String(request.resolutionViewport.west));
    query.set("resolution_north", String(request.resolutionViewport.north));
    query.set("resolution_east", String(request.resolutionViewport.east));
  }
  if (cellSizePx !== null) {
    query.set("cluster_cell_size_px", String(cellSizePx));
  }
  appendLabelFilter(query, "require_labels", request.requireLabels);
  appendLabelFilter(query, "exclude_labels", request.excludeLabels);
  return query;
}

function appendLabelFilter(
  query: URLSearchParams,
  parameter: "require_labels" | "exclude_labels",
  labels: string[] | undefined
): void {
  const value = (labels ?? [])
    .map((label) => label.trim())
    .filter(Boolean)
    .map((label) => label.replaceAll("\\", "\\\\").replaceAll(",", "\\,"))
    .join(",");
  if (value) {
    query.set(parameter, value);
  }
}

export async function getStoreIndexDelta(
  token: string,
  limit?: number
): Promise<StoreIndexDeltaResponse> {
  const query = new URLSearchParams({ token });
  if (typeof limit === "number" && Number.isFinite(limit) && limit > 0) {
    query.set("limit", String(Math.floor(limit)));
  }
  return fetchJson<StoreIndexDeltaResponse>(`${apiV1("/store/index/delta")}?${query.toString()}`);
}

export async function retryStoreMediaCacheEntry(
  key: string,
  options?: {
    snapshot?: string | null;
    version?: string | null;
    readMode?: string | null;
  }
): Promise<StoreIndexMedia> {
  const trimmedKey = key.trim();
  if (!trimmedKey) {
    throw new Error("key must not be empty");
  }

  const query = new URLSearchParams({ key: trimmedKey });
  if (options?.snapshot?.trim()) {
    query.set("snapshot", options.snapshot.trim());
  }
  if (options?.version?.trim()) {
    query.set("version", options.version.trim());
  }
  if (options?.readMode?.trim()) {
    query.set("read_mode", options.readMode.trim());
  }

  return fetchJson<StoreIndexMedia>(`${apiV1("/media/cache/retry")}?${query.toString()}`, {
    method: "POST"
  });
}

export async function getStoreValue(
  key: string,
  snapshot?: string | null,
  version?: string | null,
  previewBytes?: number | null
): Promise<StoreGetResponse> {
  const query = new URLSearchParams({ key });
  if (snapshot?.trim()) {
    query.set("snapshot", snapshot.trim());
  }
  if (version?.trim()) {
    query.set("version", version.trim());
  }
  if (previewBytes && previewBytes > 0) {
    query.set("preview_bytes", String(Math.floor(previewBytes)));
  }
  return fetchJson<StoreGetResponse>(`${apiV1("/store/get")}?${query.toString()}`);
}

export async function putStoreValue(key: string, value: string): Promise<StorePutResponse> {
  return fetchJson<StorePutResponse>(apiV1("/store/put"), {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify({ key, value })
  });
}

export async function renameStorePath(
  fromPath: string,
  toPath: string,
  overwrite = false
): Promise<JsonObject> {
  return fetchJson<JsonObject>(apiV1("/store/rename"), {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify({
      from_path: fromPath,
      to_path: toPath,
      overwrite
    })
  });
}

export async function deleteStoreValue(key: string): Promise<JsonObject> {
  const query = new URLSearchParams({ key });
  return fetchJson<JsonObject>(`${apiV1("/store/delete")}?${query.toString()}`, {
    method: "DELETE"
  });
}

export async function restoreStorePathFromSnapshot(
  snapshot: string,
  sourcePath: string,
  targetPath: string,
  recursive = false
): Promise<JsonObject> {
  return fetchJson<JsonObject>(apiV1("/store/restore"), {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify({
      snapshot,
      source_path: sourcePath,
      target_path: targetPath,
      recursive
    })
  });
}

export async function putBinaryObject(
  key: string,
  file: File,
  onProgress?: (progress: BinaryUploadProgress) => void,
  options?: BinaryUploadOptions
): Promise<StorePutResponse> {
  const signal = options?.signal;
  let session: StoreUploadSessionStartResponse | null = null;
  let completedUpload = false;

  try {
    session = await startStoreUploadSession(key, file.size, signal);
    const receivedIndexes = new Set(session.received_indexes);
    let uploadedBytes = byteCountForReceivedIndexes(
      receivedIndexes,
      session.chunk_size_bytes,
      file.size
    );
    let uploadedChunks = receivedIndexes.size;

    onProgress?.(
      buildBinaryUploadProgress(
        uploadedBytes,
        file.size,
        uploadedChunks,
        session.chunk_count,
        uploadedChunks === 0 ? "starting" : "uploading"
      )
    );

    for (let index = 0; index < session.chunk_count; index += 1) {
      if (receivedIndexes.has(index)) {
        continue;
      }

      const start = index * session.chunk_size_bytes;
      const end = Math.min(start + session.chunk_size_bytes, file.size);
      const chunk = file.slice(start, end);
      const response = await fetch(
        apiV1(`/store/uploads/${encodeURIComponent(session.upload_id)}/chunk/${index}`),
        {
          method: "PUT",
          body: chunk,
          headers: {
            "content-type": file.type || "application/octet-stream"
          },
          signal
        }
      );
      const ack = await readJsonResponse<StoreUploadSessionChunkResponse>(response);
      if (ack.received_index !== index) {
        throw new Error(`Chunk upload desynchronized at index ${index}`);
      }
      uploadedBytes += end - start;
      uploadedChunks += 1;
      onProgress?.(
        buildBinaryUploadProgress(
          uploadedBytes,
          file.size,
          uploadedChunks,
          session.chunk_count,
          "uploading"
        )
      );
    }

    onProgress?.(
      buildBinaryUploadProgress(
        file.size,
        file.size,
        session.chunk_count,
        session.chunk_count,
        "finalizing"
      )
    );
    const completed = await completeStoreUploadSession(session.upload_id, signal);
    completedUpload = true;
    onProgress?.(
      buildBinaryUploadProgress(
        completed.total_size_bytes,
        completed.total_size_bytes,
        session.chunk_count,
        session.chunk_count,
        "complete"
      )
    );
    return {
      key: session.key,
      size_bytes: completed.total_size_bytes,
      upload_mode: "chunked",
      chunk_size_bytes: session.chunk_size_bytes,
      chunk_count: session.chunk_count
    };
  } catch (error) {
    if (session && !completedUpload && isAbortError(error)) {
      await deleteStoreUploadSession(session.upload_id).catch(() => undefined);
    }
    throw error;
  }
}

export function getBinaryObjectDownloadUrl(
  key: string,
  snapshot?: string | null,
  version?: string | null
): string {
  return buildBinaryObjectUrl(apiV1("/store/get-binary"), key, snapshot, version);
}

export function getBinaryObjectStreamUrl(
  key: string,
  snapshot?: string | null,
  version?: string | null
): string {
  return buildBinaryObjectUrl(apiV1("/store/stream-binary"), key, snapshot, version);
}

function buildBinaryObjectUrl(
  basePath: string,
  key: string,
  snapshot?: string | null,
  version?: string | null
): string {
  const query = new URLSearchParams({ key });
  if (snapshot?.trim()) {
    query.set("snapshot", snapshot.trim());
  }
  if (version?.trim()) {
    query.set("version", version.trim());
  }
  return `${basePath}?${query.toString()}`;
}

export async function downloadBinaryObject(
  key: string,
  snapshot?: string | null,
  version?: string | null
): Promise<BinaryDownloadResult> {
  const response = await fetch(getBinaryObjectDownloadUrl(key, snapshot, version));
  if (!response.ok) {
    throw new Error(await readErrorMessage(response));
  }

  const contentDisposition = response.headers.get("content-disposition") || "";
  const filenameMatch = contentDisposition.match(/filename="([^"]+)"/i);
  return {
    blob: await response.blob(),
    filename: filenameMatch?.[1] || key.split("/").pop() || "download.bin",
    contentType: response.headers.get("content-type") || "application/octet-stream"
  };
}

export async function getVersionGraph(key: string): Promise<VersionGraphResponse> {
  return fetchJson<VersionGraphResponse>(
    `${apiV1("/versions")}?key=${encodeURIComponent(key)}`
  );
}

export async function restoreStoreVersion(
  key: string,
  versionId: string,
  targetPath: string
): Promise<JsonObject | null> {
  return fetchJson<JsonObject | null>(
    `${apiV1("/versions")}/${encodeURIComponent(key)}/restore/${encodeURIComponent(versionId)}`,
    {
      method: "POST",
      headers: {
        "content-type": "application/json"
      },
      body: JSON.stringify({
        to_path: targetPath,
        overwrite: false
      })
    }
  );
}

async function readJsonResponse<T>(response: Response): Promise<T> {
  if (!response.ok) {
    throw new Error(await readErrorMessage(response));
  }
  return (await response.json()) as T;
}

async function startStoreUploadSession(
  key: string,
  totalSizeBytes: number,
  signal?: AbortSignal
): Promise<StoreUploadSessionStartResponse> {
  return fetchJson<StoreUploadSessionStartResponse>(apiV1("/store/uploads/start"), {
    method: "POST",
    headers: {
      "content-type": "application/json"
    },
    body: JSON.stringify({
      key,
      total_size_bytes: totalSizeBytes
    }),
    signal
  });
}

async function completeStoreUploadSession(
  uploadId: string,
  signal?: AbortSignal
): Promise<StoreUploadSessionCompleteResponse> {
  return fetchJson<StoreUploadSessionCompleteResponse>(
    apiV1(`/store/uploads/${encodeURIComponent(uploadId)}/complete`),
    {
      method: "POST",
      signal
    }
  );
}

async function deleteStoreUploadSession(
  uploadId: string,
  signal?: AbortSignal
): Promise<void> {
  const response = await fetch(apiV1(`/store/uploads/${encodeURIComponent(uploadId)}`), {
    method: "DELETE",
    signal
  });
  if (response.ok || response.status === 404 || response.status === 409) {
    return;
  }
  throw new Error(await readErrorMessage(response));
}

function byteCountForReceivedIndexes(
  receivedIndexes: Set<number>,
  chunkSizeBytes: number,
  totalSizeBytes: number
): number {
  let uploadedBytes = 0;
  for (const index of receivedIndexes) {
    const start = index * chunkSizeBytes;
    const end = Math.min(start + chunkSizeBytes, totalSizeBytes);
    uploadedBytes += Math.max(0, end - start);
  }
  return uploadedBytes;
}

function buildBinaryUploadProgress(
  uploadedBytes: number,
  totalBytes: number,
  uploadedChunks: number,
  totalChunks: number,
  phase: BinaryUploadProgress["phase"]
): BinaryUploadProgress {
  const safeTotalBytes = Math.max(0, totalBytes);
  const safeUploadedBytes = Math.min(Math.max(0, uploadedBytes), safeTotalBytes);
  const percent =
    safeTotalBytes === 0 ? 100 : Math.round((safeUploadedBytes / safeTotalBytes) * 100);
  return {
    uploadedBytes: safeUploadedBytes,
    totalBytes: safeTotalBytes,
    uploadedChunks,
    totalChunks,
    percent,
    phase
  };
}

async function readErrorMessage(response: Response): Promise<string> {
  const payload = await response.json().catch(() => null);
  if (payload && typeof payload === "object" && "error" in payload && typeof payload.error === "string") {
    return payload.error;
  }
  return `HTTP ${response.status}`;
}

function isAbortError(error: unknown): boolean {
  return (
    error instanceof DOMException && error.name === "AbortError"
  ) || (
    error instanceof Error && error.name === "AbortError"
  );
}
