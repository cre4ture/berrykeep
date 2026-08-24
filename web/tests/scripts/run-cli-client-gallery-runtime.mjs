import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { spawn } from "node:child_process";
import { cargoDebugBinaryPath } from "./cargo-target.mjs";

const repoRoot = resolve(process.cwd(), "..");
const binaryPath = cargoDebugBinaryPath(repoRoot, "ironmesh");
const webUiPort = 18081;
const upstreamPort = 18082;
const webUiBindAddress = process.env.IRONMESH_GALLERY_RUNTIME_BIND ?? "127.0.0.1";
const upstreamOrigin = `http://127.0.0.1:${upstreamPort}`;
const fallbackMapManifestKey = "sys/maps/runtime-gallery-fallback.mbtiles.manifest.json";
const fallbackMapPartKey = "sys/maps/runtime-gallery-fallback.mbtiles.part-0000";
const tinyPngBody = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9WlH7u8AAAAASUVORK5CYII=",
  "base64"
);
const localCenterMapBody = createLocalCenterMapBody(readFileSync("tests/fixtures/smoke.mbtiles"));
const fallbackMapManifest = {
  manifest_version: 1,
  type: "split_file_manifest",
  logical_format: "mbtiles",
  logical_key: "sys/maps/runtime-gallery-fallback.mbtiles",
  logical_size_bytes: localCenterMapBody.length,
  parts_count: 1,
  parts: [
    {
      part_id: "part-0000",
      key: fallbackMapPartKey,
      offset_bytes: 0,
      size_bytes: localCenterMapBody.length
    }
  ]
};

const mapConfiguration = {
  stored: true,
  configuration: {
    version: 1,
    active_variant_id: "natural-earth-globe",
    variants: [
      {
        id: "natural-earth-globe",
        label: "Natural Earth Globe",
        mode_label: "Globe",
        description: "Small global overview map.",
        attribution: "Made with Natural Earth.",
        kind: "raster",
        style: "raster",
        enabled: true,
        raster_manifest_key: fallbackMapManifestKey
      },
      {
        id: "natural-earth-labels",
        label: "Natural Earth Globe + labels",
        mode_label: "Labels",
        description: "Natural Earth base map with country, city, and border labels.",
        attribution: "Made with Natural Earth.",
        kind: "raster",
        style: "raster",
        enabled: true,
        raster_manifest_key: fallbackMapManifestKey
      },
      {
        id: "openmaptiles-street",
        label: "OpenMapTiles Street",
        mode_label: "Street",
        description: "Detailed global OpenMapTiles street map.",
        attribution: "Map data © OpenStreetMap contributors.",
        kind: "raster",
        style: "raster",
        enabled: true,
        raster_manifest_key: fallbackMapManifestKey
      }
    ]
  }
};

const galleryEntries = [
  createGalleryEntry("runtime-map-a", 40.7128, -74.006),
  createGalleryEntry("runtime-map-b", 40.7628, -74.056),
  createGalleryEntry("runtime-tokyo-a", 35.6762, 139.6503),
  createGalleryEntry("runtime-tokyo-b", 35.7262, 139.7003)
];

const galleryMapClusters = [
  {
    cluster_id: "runtime-new-york",
    count: 2,
    latitude: 40.7378,
    longitude: -74.031,
    bounds: { south: 40.7128, west: -74.056, north: 40.7628, east: -74.006 },
    paths: ["gallery/runtime-map-a.png", "gallery/runtime-map-b.png"]
  },
  {
    cluster_id: "runtime-tokyo",
    count: 2,
    latitude: 35.7012,
    longitude: 139.6752,
    bounds: { south: 35.6762, west: 139.6503, north: 35.7262, east: 139.7003 },
    paths: ["gallery/runtime-tokyo-a.png", "gallery/runtime-tokyo-b.png"]
  }
];

function galleryIndexResponse(url) {
  const prefix = url.searchParams.get("prefix") ?? "";
  const depth = Math.max(1, Number(url.searchParams.get("depth") ?? "1") || 1);
  const offset = Math.max(0, Number(url.searchParams.get("offset") ?? "0") || 0);
  const limitParam = url.searchParams.get("limit");
  const limit = limitParam === null ? null : Math.max(1, Number(limitParam) || 1);
  const scopedEntries = galleryEntries.filter((entry) => entry.path.startsWith(prefix));
  const entries =
    limit === null ? scopedEntries.slice(offset) : scopedEntries.slice(offset, offset + limit);

  return {
    prefix,
    depth,
    entry_count: entries.length,
    total_entry_count: scopedEntries.length,
    offset,
    limit,
    has_more: offset + entries.length < scopedEntries.length,
    consistency_token: "gallery-runtime-revision-1",
    media_summary: {
      ready_count: scopedEntries.length,
      pending_count: 0,
      incomplete_count: 0,
      image_count: scopedEntries.length,
      video_count: 0,
      geotagged_count: scopedEntries.length
    },
    entries
  };
}

function galleryMapClustersResponse(url) {
  const prefix = url.searchParams.get("prefix") ?? "";
  const depth = Math.max(1, Number(url.searchParams.get("depth") ?? "1") || 1);
  const zoom = Math.max(0, Math.min(20, Number(url.searchParams.get("zoom") ?? "1") || 0));
  const viewport = {
    south: Number(url.searchParams.get("south")),
    west: Number(url.searchParams.get("west")),
    north: Number(url.searchParams.get("north")),
    east: Number(url.searchParams.get("east"))
  };
  const clusters = galleryMapClusters.filter((cluster) => clusterIsVisible(cluster, viewport));
  const visibleGeotaggedCount = clusters.reduce((total, cluster) => total + cluster.count, 0);
  return {
    prefix,
    depth,
    zoom,
    resolution: 2 ** (Math.floor(zoom) + 2),
    total_entry_count: galleryEntries.length,
    visible_geotagged_count: visibleGeotaggedCount,
    media_summary: {
      ready_count: galleryEntries.length,
      pending_count: 0,
      incomplete_count: 0,
      image_count: galleryEntries.length,
      video_count: 0,
      geotagged_count: galleryEntries.length
    },
    query_token: "gallery-runtime-map-token-1",
    clusters: clusters.map(({ paths: _paths, ...cluster }) => cluster)
  };
}

function galleryMapClusterEntriesResponse(url) {
  const offset = Math.max(0, Number(url.searchParams.get("offset") ?? "0") || 0);
  const limit = Math.max(1, Number(url.searchParams.get("limit") ?? "100") || 100);
  const clusterId = url.searchParams.get("cluster_id") ?? "";
  const cluster = galleryMapClusters.find((candidate) => candidate.cluster_id === clusterId);
  const clusterEntries = cluster
    ? galleryEntries.filter((entry) => cluster.paths.includes(entry.path))
    : [];
  const entries = clusterEntries.slice(offset, offset + limit);
  return {
    cluster_id: clusterId,
    entry_count: entries.length,
    total_entry_count: clusterEntries.length,
    offset,
    limit,
    has_more: offset + entries.length < clusterEntries.length,
    query_token: url.searchParams.get("query_token") ?? "gallery-runtime-map-token-1",
    entries
  };
}

function createGalleryEntry(name, latitude, longitude) {
  const path = `gallery/${name}.png`;
  return {
    path,
    entry_type: "key",
    version: `${name}-001`,
    content_hash: `${name}-hash`,
    size_bytes: 68,
    modified_at_unix: 1712345678,
    media: {
      status: "ready",
      content_fingerprint: `${name}-fingerprint`,
      media_type: "image",
      mime_type: "image/png",
      width: 1,
      height: 1,
      taken_at_unix: 1712345678,
      gps: { latitude, longitude },
      thumbnail: {
        url: `/media/thumbnail?key=${encodeURIComponent(path)}`,
        profile: "grid",
        width: 1,
        height: 1,
        format: "png",
        size_bytes: 68
      }
    }
  };
}

function clusterIsVisible(cluster, viewport) {
  if (!Object.values(viewport).every(Number.isFinite)) {
    return true;
  }
  return (
    cluster.bounds.north >= viewport.south &&
    cluster.bounds.south <= viewport.north &&
    cluster.bounds.east >= viewport.west &&
    cluster.bounds.west <= viewport.east
  );
}

function createLocalCenterMapBody(source) {
  const mapBody = Buffer.from(source);
  const originalCenter = Buffer.from("0,20,1");
  const localCenter = Buffer.from("8,47,3");
  const offset = mapBody.indexOf(originalCenter);
  if (offset < 0 || mapBody.indexOf(originalCenter, offset + 1) >= 0) {
    throw new Error("runtime map fixture must contain exactly one default center value");
  }
  localCenter.copy(mapBody, offset);
  return mapBody;
}

function json(response, status, body) {
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store"
  });
  response.end(JSON.stringify(body));
}

function binary(response, status, body, contentType) {
  response.writeHead(status, {
    "content-type": contentType,
    "content-length": String(body.length),
    "accept-ranges": "bytes"
  });
  response.end(body);
}

function upstreamRequest(request, response) {
  const url = new URL(request.url ?? "/", upstreamOrigin);
  if (request.method === "GET" && url.pathname === "/api/v1/maps/config") {
    json(response, 200, mapConfiguration);
    return;
  }
  if (request.method === "GET" && url.pathname.startsWith("/api/v1/store/")) {
    const key = decodeURIComponent(url.pathname.slice("/api/v1/store/".length));
    if (key === fallbackMapManifestKey) {
      json(response, 200, fallbackMapManifest);
      return;
    }
    if (key === fallbackMapPartKey) {
      binary(response, 200, localCenterMapBody, "application/octet-stream");
      return;
    }
  }
  if (request.method === "GET" && url.pathname === "/api/v1/snapshots") {
    json(response, 200, []);
    return;
  }
  if (request.method === "GET" && url.pathname === "/api/v1/store/index") {
    json(response, 200, galleryIndexResponse(url));
    return;
  }
  if (request.method === "GET" && url.pathname === "/api/v1/store/map/clusters") {
    json(response, 200, galleryMapClustersResponse(url));
    return;
  }
  if (request.method === "GET" && url.pathname === "/api/v1/store/map/cluster-entries") {
    json(response, 200, galleryMapClusterEntriesResponse(url));
    return;
  }
  if (request.method === "GET" && url.pathname === "/api/v1/media/thumbnail") {
    binary(response, 200, tinyPngBody, "image/png");
    return;
  }
  json(response, 404, { message: `runtime fixture has no route for ${request.method} ${url.pathname}` });
}

const upstream = createServer(upstreamRequest);
let clientProcess;
let shuttingDown = false;

function finish(exitCode) {
  upstream.close(() => process.exit(exitCode));
}

function stop(signal) {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  if (clientProcess && !clientProcess.killed) {
    clientProcess.once("exit", () => finish(0));
    clientProcess.kill(signal);
    return;
  }
  finish(0);
}

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => stop(signal));
}

upstream.listen(upstreamPort, "127.0.0.1", () => {
  clientProcess = spawn(
    binaryPath,
    [
      "--server-base-url",
      upstreamOrigin,
      "serve-web",
      "--bind",
      `${webUiBindAddress}:${webUiPort}`
    ],
    {
      cwd: repoRoot,
      env: {
        ...process.env,
        RUST_LOG: process.env.RUST_LOG ?? "info"
      },
      stdio: "inherit"
    }
  );

  clientProcess.on("exit", (code, signal) => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    finish(signal ? 1 : (code ?? 1));
  });
  clientProcess.on("error", (error) => {
    if (shuttingDown) {
      return;
    }
    console.error(`failed to start client runtime: ${error.message}`);
    shuttingDown = true;
    finish(1);
  });
});
