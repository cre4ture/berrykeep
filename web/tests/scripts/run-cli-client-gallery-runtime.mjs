import { createServer } from "node:http";
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
const tinyPngBody = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9WlH7u8AAAAASUVORK5CYII=",
  "base64"
);

// Android's software WebView emulator does not initialize MapLibre reliably.
// The mobile fullscreen regression is renderer-independent, so exercise the
// built-in atlas fallback and its shared fullscreen/modal behavior here.
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
  {
    path: "gallery/runtime-map-a.png",
    entry_type: "key",
    version: "runtime-map-a-001",
    content_hash: "runtime-map-a-hash",
    size_bytes: 68,
    modified_at_unix: 1712345678,
    media: {
      status: "ready",
      content_fingerprint: "runtime-map-a-fingerprint",
      media_type: "image",
      mime_type: "image/png",
      width: 1,
      height: 1,
      taken_at_unix: 1712345678,
      gps: {
        latitude: 47.3769,
        longitude: 8.5417
      },
      thumbnail: {
        url: "/media/thumbnail?key=gallery%2Fruntime-map-a.png",
        profile: "grid",
        width: 1,
        height: 1,
        format: "png",
        size_bytes: 68
      }
    }
  },
  {
    path: "gallery/runtime-map-b.png",
    entry_type: "key",
    version: "runtime-map-b-001",
    content_hash: "runtime-map-b-hash",
    size_bytes: 68,
    modified_at_unix: 1712345678,
    media: {
      status: "ready",
      content_fingerprint: "runtime-map-b-fingerprint",
      media_type: "image",
      mime_type: "image/png",
      width: 1,
      height: 1,
      taken_at_unix: 1712345678,
      // Identical coordinates intentionally exercise the direct cluster
      // chooser instead of the basemap's zoom-to-bounds behavior.
      gps: {
        latitude: 47.3769,
        longitude: 8.5417
      },
      thumbnail: {
        url: "/media/thumbnail?key=gallery%2Fruntime-map-b.png",
        profile: "grid",
        width: 1,
        height: 1,
        format: "png",
        size_bytes: 68
      }
    }
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
  if (request.method === "GET" && url.pathname === "/api/v1/snapshots") {
    json(response, 200, []);
    return;
  }
  if (request.method === "GET" && url.pathname === "/api/v1/store/index") {
    json(response, 200, galleryIndexResponse(url));
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
