import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { gzipSync } from "node:zlib";
import { expect, test, type Locator, type Page, type Route } from "@playwright/test";
import {
  createInitialOverviewGalleryEntries,
  createLiveClusterGalleryEntries,
  registerGalleryMapContractTests
} from "./gallery-map.contract";
import { GalleryMapMockSession } from "./gallery-map.mock";
import {
  filterMockStoreEntriesToPrefix,
  projectMockStoreTreeEntries
} from "./store-index.mock";

const API_V1_PREFIX = "/api/v1";
const HOME_NAS_NODE_ID = "0198e5b8-8bb4-7cc0-a6d7-8648251845b8";
const EMPTY_WEB_SERVICE_NODE_ID = "0198e5b8-8bb4-7cc0-a6d7-8648251845b9";
const CONCURRENT_WEB_SERVICE_NODE_IDS = [
  "0198e5b8-8bb4-7cc0-a6d7-8648251845c0",
  "0198e5b8-8bb4-7cc0-a6d7-8648251845c1",
  "0198e5b8-8bb4-7cc0-a6d7-8648251845c2",
  "0198e5b8-8bb4-7cc0-a6d7-8648251845c3",
  "0198e5b8-8bb4-7cc0-a6d7-8648251845c4"
];

function apiV1(path: string): string {
  return `${API_V1_PREFIX}${path}`;
}

function createDeferred(): { promise: Promise<void>; resolve: () => void } {
  let resolvePromise!: () => void;
  const promise = new Promise<void>((resolve) => {
    resolvePromise = () => resolve();
  });
  return { promise, resolve: resolvePromise };
}

registerGalleryMapContractTests({
  name: "client-ui",
  setup: (page, options) =>
    installClientUiMocks(page, {
      mapConfiguration: options.mapConfiguration,
      mapConfigurationStatus: options.mapConfigurationStatus
    }),
  setupInitialOverviewScenario: (page) =>
    installClientUiMocks(page, {
      storeEntries: createInitialOverviewGalleryEntries(),
      mapMetadataCenter: [8.5417, 47.3769, 3]
    }),
  setupLiveClusterScenario: (page) =>
    installClientUiMocks(page, {
      storeEntries: createLiveClusterGalleryEntries(),
      mapMetadataCenter: [8.5417, 47.3769, 3]
    }),
  openGallery: async (page) => {
    await page.goto("/");
    await page.getByText("Gallery", { exact: true }).click();
    await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
  }
});

test("private service origins keep the launch cookie and sibling sites isolated", async ({
  page
}) => {
  const server = createServer((request, response) => {
    const host = String(request.headers.host ?? "");
    const address = server.address();
    if (!address || typeof address === "string") {
      response.writeHead(500).end("listener address is unavailable");
      return;
    }
    if (host.startsWith("localhost:") && request.url === "/start") {
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.end(
        `<a id="launch" href="http://strict-check.localhost:${address.port}/_ironmesh/open">Open</a>`
      );
      return;
    }
    if (
      host.startsWith("strict-check.localhost:") &&
      request.url === "/_ironmesh/open"
    ) {
      response.setHeader(
        "set-cookie",
        "ironmesh_service_gateway_session=session-secret; HttpOnly; SameSite=Strict; Path=/"
      );
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.setHeader(
        "content-security-policy",
        "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
      );
      response.end(
        '<!doctype html><meta http-equiv="refresh" content="0;url=/"><a href="/">Continue</a>'
      );
      return;
    }
    if (host.startsWith("sibling.localhost:") && request.url === "/attack") {
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.end(
        `<script>
          document.cookie = "ironmesh_service_gateway_session=shadow; Domain=localhost; Path=/";
          document.cookie = "sid=sibling-injected; Domain=localhost; Path=/";
        </script>
        <a id="cross-service" href="http://strict-check.localhost:${address.port}/">Open sibling</a>`
      );
      return;
    }
    if (host.startsWith("strict-check.localhost:") && request.url === "/") {
      const cookies = String(request.headers.cookie ?? "");
      const authenticated = cookies.includes(
        "ironmesh_service_gateway_session=session-secret"
      );
      const siblingCookieLeaked =
        cookies.includes("ironmesh_service_gateway_session=shadow") ||
        cookies.includes("sid=sibling-injected");
      const body = siblingCookieLeaked
        ? "sibling-cookie-leaked"
        : authenticated
          ? "authenticated-first-landing"
          : "unauthenticated-cross-site";
      response.writeHead(authenticated ? 200 : 401).end(body);
      return;
    }
    response.writeHead(404).end("not-found");
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  try {
    const address = server.address();
    if (!address || typeof address === "string") {
      throw new Error("listener address is unavailable");
    }
    await page.goto(`http://localhost:${address.port}/start`);
    await page.locator("#launch").click();
    await page.waitForURL(
      (url) => url.hostname === "strict-check.localhost" && url.pathname === "/"
    );
    await expect(page.locator("body")).toHaveText("authenticated-first-landing");

    await page.goto(`http://sibling.localhost:${address.port}/attack`);
    await page.locator("#cross-service").click();
    await page.waitForURL(
      (url) => url.hostname === "strict-check.localhost" && url.pathname === "/"
    );
    await expect(page.locator("body")).toHaveText("unauthenticated-cross-site");
  } finally {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }
});

test("a blocked service popup leaves the client UI open", async ({ page }) => {
  await page.addInitScript(() => {
    window.open = () => null;
  });
  await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Web services" })).toBeVisible();

  await page.getByRole("button", { name: "Open in browser" }).click();

  await expect(
    page.getByText("The browser blocked the service popup. Allow popups and try again.")
  ).toBeVisible();
  await expect(page.getByRole("heading", { name: "Web services" })).toBeVisible();
});

test("the embedded Android client offers in-app and external private-service handoff", async ({ page }) => {
  await installClientUiMocks(page);
  await page.goto("/?embedded_client=android");
  await page.getByText("Web services", { exact: true }).click();

  await expect(page.getByRole("button", { name: "Open in BerryKeep" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Open in browser" })).toBeVisible();
});

test("web service results appear while another node is still checking", async ({ page }) => {
  const slowNodeStarted = createDeferred();
  const releaseSlowNode = createDeferred();
  await installClientUiMocks(page);
  await page.route(
    `**${apiV1(`/web-services/nodes/${EMPTY_WEB_SERVICE_NODE_ID}`)}`,
    async (route) => {
      slowNodeStarted.resolve();
      await releaseSlowNode.promise;
      await json(route, {
        nodeId: EMPTY_WEB_SERVICE_NODE_ID,
        available: true,
        services: []
      });
    }
  );

  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();
  await slowNodeStarted.promise;

  await expect(page.getByText("Home NAS", { exact: true })).toBeVisible();
  await expect(page.getByText("1 node is still checking.", { exact: true })).toBeVisible();

  releaseSlowNode.resolve();
  await expect(page.getByText("Checked 2 of 2 nodes", { exact: true })).toBeVisible();
  await expect(page.getByText("No services for this device", { exact: true })).toBeVisible();
});

test("web service discovery acknowledges an empty response before all nodes finish", async ({ page }) => {
  const slowNodeStarted = createDeferred();
  const releaseSlowNode = createDeferred();
  await installClientUiMocks(page);
  await page.route(
    `**${apiV1(`/web-services/nodes/${HOME_NAS_NODE_ID}`)}`,
    (route) =>
      json(route, {
        nodeId: HOME_NAS_NODE_ID,
        available: true,
        services: []
      })
  );
  await page.route(
    `**${apiV1(`/web-services/nodes/${EMPTY_WEB_SERVICE_NODE_ID}`)}`,
    async (route) => {
      slowNodeStarted.resolve();
      await releaseSlowNode.promise;
      await json(route, {
        nodeId: EMPTY_WEB_SERVICE_NODE_ID,
        available: true,
        services: []
      });
    }
  );

  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();
  await slowNodeStarted.promise;

  await expect(page.getByText("No services returned yet", { exact: true })).toBeVisible();
  await expect(page.getByText("1 node is still checking.", { exact: true })).toBeVisible();

  releaseSlowNode.resolve();
  await expect(
    page.getByText("No services available to this device", { exact: true })
  ).toBeVisible();
});

test("web service discovery distinguishes unreachable nodes from empty service lists", async ({
  page
}) => {
  await installClientUiMocks(page);
  await page.route(
    `**${apiV1(`/web-services/nodes/${HOME_NAS_NODE_ID}`)}`,
    (route) =>
      json(route, {
        nodeId: HOME_NAS_NODE_ID,
        available: false,
        services: []
      })
  );
  await page.route(
    `**${apiV1(`/web-services/nodes/${EMPTY_WEB_SERVICE_NODE_ID}`)}`,
    (route) =>
      json(route, {
        nodeId: EMPTY_WEB_SERVICE_NODE_ID,
        available: false,
        services: []
      })
  );

  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();

  await expect(page.getByText("No node could be reached", { exact: true })).toBeVisible();
  await expect(
    page.getByText(
      "No node has returned an available web-service list yet. Check the connection to the listed nodes and retry.",
      { exact: true }
    )
  ).toBeVisible();
});

test("web service discovery limits concurrent node requests", async ({ page }) => {
  const releaseResponses = createDeferred();
  let activeRequests = 0;
  let maxConcurrentRequests = 0;
  await installClientUiMocks(page);
  await page.route(`**${apiV1("/web-services/nodes")}`, (route) =>
    json(route, { nodeIds: CONCURRENT_WEB_SERVICE_NODE_IDS })
  );
  await page.route(`**${apiV1("/web-services/nodes/")}*`, async (route) => {
    activeRequests += 1;
    maxConcurrentRequests = Math.max(maxConcurrentRequests, activeRequests);
    await releaseResponses.promise;
    activeRequests -= 1;
    const nodeId = route.request().url().split("/").pop()!;
    await json(route, { nodeId, available: true, services: [] });
  });

  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();

  await expect.poll(() => activeRequests).toBe(4);
  releaseResponses.resolve();
  await expect(page.getByText("Checked 5 of 5 nodes", { exact: true })).toBeVisible();
  expect(maxConcurrentRequests).toBe(4);
});

test("web service discovery falls back when node discovery is unavailable", async ({ page }) => {
  await installClientUiMocks(page);
  await page.route(`**${apiV1("/web-services/nodes")}`, (route) =>
    route.fulfill({ status: 404 })
  );

  await page.goto("/");
  await page.getByText("Web services", { exact: true }).click();

  await expect(page.getByText("Home NAS", { exact: true })).toBeVisible();
  await expect(page.getByText("Web service request failed", { exact: true })).not.toBeVisible();
});

async function dispatchCtrlWheel(locator: Locator, deltaY: number): Promise<void> {
  await locator.evaluate((element, wheelDelta) => {
    const rect = element.getBoundingClientRect();
    element.dispatchEvent(
      new WheelEvent("wheel", {
        bubbles: true,
        cancelable: true,
        ctrlKey: true,
        deltaY: wheelDelta,
        clientX: rect.left + rect.width / 2,
        clientY: rect.top + rect.height / 2
      })
    );
  }, deltaY);
}

async function installAndroidShareBridgeMock(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const messages: string[] = [];
    const listeners = new Set<(event: { data: string }) => void>();
    Object.assign(window, {
      __ironmeshShareMessages: messages,
      IronmeshAndroidShare: {
        postMessage(message: string) {
          messages.push(message);
          const request = JSON.parse(message) as { requestId: string };
          queueMicrotask(() => {
            const response = JSON.stringify({ requestId: request.requestId, status: "opened" });
            listeners.forEach((listener) => listener({ data: response }));
          });
        },
        addEventListener(_type: "message", listener: (event: { data: string }) => void) {
          listeners.add(listener);
        },
        removeEventListener(_type: "message", listener: (event: { data: string }) => void) {
          listeners.delete(listener);
        }
      }
    });
  });
}

async function androidShareMessages(page: Page): Promise<string[]> {
  return page.evaluate(
    () => (window as typeof window & { __ironmeshShareMessages: string[] }).__ironmeshShareMessages
  );
}

test("client-ui smoke flow renders and performs core operations", async ({ page }) => {
  test.setTimeout(45_000);
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], {
    origin: "http://127.0.0.1:4174"
  });
  const uploadMetrics = await installClientUiMocks(page);
  const pageErrors: string[] = [];

  page.on("pageerror", (error) => {
    pageErrors.push(error.message);
  });

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
  await expect(page.getByRole("banner").getByText("cli-client-web", { exact: true })).toBeVisible();
  await expect(page.getByText("Transport-aware", { exact: true })).toBeVisible();
  await expect(page.getByText("Version info", { exact: true })).toBeVisible();
  await expect(page.getByText("Device identity", { exact: true })).toBeVisible();
  await expect(page.getByText("dashboard-device", { exact: true })).toBeVisible();
  await expect(page.getByText("device-dashboard-001", { exact: true })).toBeVisible();
  await expect(page.getByText("Rendezvous mTLS available", { exact: true })).toBeVisible();
  await expect(page.getByText(/UI build:\s*\S+\s+\(.+\)/)).toBeVisible();
  await expect(page.getByText("Backend build: 0.1.0 (v0.1.0-3-gmocked)")).toBeVisible();
  await expect(page.getByText("Active route")).toBeVisible();
  await expect(page.getByText("Direct", { exact: true })).toBeVisible();
  await expect(page.getByText("node-alpha", { exact: true })).toBeVisible();
  await expect(page.getByText("https://node-alpha.local", { exact: true })).toBeVisible();
  expect(uploadMetrics.diagnosticContexts()).toHaveLength(1);
  expect(uploadMetrics.diagnosticContexts()[0]).toMatch(/^overview-refresh-\d+-\d+$/);
  expect(uploadMetrics.diagnosticContextRequestCount()).toBe(5);
  await page.getByText("Connection paths", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Connection Paths" })).toBeVisible();
  await expect(page.getByText("Overall search state")).toBeVisible();
  await expect(page.getByText("Multiple routes active")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Direct HTTPS to node-alpha" }).first()).toBeVisible();
  await expect(page.getByText("Relay via rendezvous-a.local:9443 to node-alpha", { exact: true })).toBeVisible();
  await expect(page.getByText("Relay via rendezvous-b.local:9443 to node-alpha", { exact: true })).toBeVisible();
  await expect(page.getByText("Hole punching", { exact: true })).toBeVisible();
  await expect(page.getByText("direct path", { exact: true }).first()).toBeVisible();
  await page.getByText("Web services", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Web services" })).toBeVisible();
  await expect(page.getByText("Home NAS", { exact: true })).toBeVisible();
  const popupPromise = page.waitForEvent("popup");
  await page.getByRole("button", { name: "Open in browser" }).click();
  const servicePopup = await popupPromise;
  await expect.poll(() => servicePopup.url()).toBe("about:blank#home-nas");
  await servicePopup.close();
  await page.getByText("Logs", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Logs" })).toBeVisible();
  await expect(page.getByText("Recent client runtime logs", { exact: true })).toBeVisible();
  await expect(page.getByText("2023-11-14T22:13:20.000Z INFO client_sdk client transport ready")).toBeVisible();
  await expect(
    page.getByText(
      "2023-11-14T22:13:22.000Z ERROR web_ui_backend health request failed: failed connecting to https://node-alpha.local/api/v1/health"
    )
  ).toBeVisible();
  await expect(page.getByText("2023-11-14T22:13:22.000Z caused by: connection refused")).toBeVisible();

  await page.getByText("Settings", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByText("Diagnostics", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Copy last 3 minutes" }).click();
  await expect(page.getByRole("button", { name: "Diagnostic log copied" })).toBeVisible();

  await page.getByText("Store", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Store" })).toBeVisible();
  await page.getByRole("button", { name: "Upload text object" }).click();
  await expect(page.getByText('"key": "docs/readme.txt"')).toBeVisible();
  await page.locator('input[type="file"]').setInputFiles([
    {
      name: "alpha.bin",
      mimeType: "application/octet-stream",
      buffer: Buffer.alloc(40, 0x61)
    },
    {
      name: "beta.bin",
      mimeType: "application/octet-stream",
      buffer: Buffer.alloc(32, 0x62)
    }
  ]);
  await page.getByRole("button", { name: "Add files to queue" }).click();
  await expect(page.getByText("images/alpha.bin", { exact: true })).toBeVisible();
  await expect(page.getByText("images/beta.bin", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: /Uploads 0\/2|Uploads 1\/2|Uploads 2\/2/ })).toBeVisible();
  await expect(page.getByText(/Starting|Uploading/).first()).toBeVisible();
  await page
    .getByRole("row", { name: /alpha\.bin/ })
    .getByRole("button", { name: "Cancel" })
    .click();
  await expect(page.getByRole("row", { name: /alpha\.bin/ })).toContainText("Canceled");
  await page.locator('input[type="file"]').setInputFiles({
    name: "gamma.bin",
    mimeType: "application/octet-stream",
    buffer: Buffer.alloc(16, 0x63)
  });
  await expect(page.getByRole("button", { name: "Add files to queue" })).toBeEnabled();
  await page.getByRole("button", { name: "Add files to queue" }).click();
  await expect(page.getByText("images/gamma.bin", { exact: true })).toBeVisible();
  await page.getByText("Cluster", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Cluster" })).toBeVisible();
  await expect(page.getByRole("button", { name: /Uploads \d\/3/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /Uploads 2\/3.*1 canceled/ })).toBeVisible();
  await page.getByRole("button", { name: /Uploads 2\/3.*1 canceled/ }).click();
  await expect(page.getByRole("heading", { name: "Store" })).toBeVisible();
  await expect(page.getByText('"operation": "binary-upload-queue"')).toBeVisible();
  await expect(page.getByText('"active_concurrency": 2')).toBeVisible();
  await expect(page.getByText('"completed_files": 2')).toBeVisible();
  await expect(page.getByText('"canceled_files": 1')).toBeVisible();
  expect(uploadMetrics.maxConcurrentUploadIds()).toBeGreaterThan(1);
  expect(uploadMetrics.deletedUploadSessionIds()).toContain("upload-1");
  await page.getByRole("button", { name: "Download text object" }).click();
  await expect(page.getByLabel("Downloaded payload")).toHaveValue("hello from the mocked store");

  await page.getByText("Explorer", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Explorer" })).toBeVisible();
  const explorerTable = page.getByRole("table").first();
  await expect(explorerTable.getByRole("columnheader", { name: /Size/ })).toBeVisible();
  await expect(explorerTable.getByRole("columnheader", { name: /Modified/ })).toBeVisible();
  await expect(page.getByRole("cell", { name: "docs/readme.txt" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "23 B" })).toBeVisible();
  await page.getByRole("row", { name: /gallery\/cat\.png/ }).getByRole("button", { name: "History" }).click();
  await expect(page.getByLabel("Key")).toHaveValue("gallery/cat.png");
  const versionHistoryTable = page.getByRole("table").nth(1);
  await expect(
    versionHistoryTable.getByRole("cell", { name: "version-cat-001", exact: true })
  ).toBeVisible();
  await expect(versionHistoryTable.getByRole("row", { name: /version-cat-001/ })).toContainText("3.0 MB");
  await expect(
    versionHistoryTable.getByRole("row", { name: /version-cat-001/ }).getByRole("button", { name: "Restore" })
  ).toHaveCount(0);
  page.once("dialog", (dialog) => {
    void dialog.accept("gallery/cat.png");
  });
  await versionHistoryTable
    .getByRole("row", { name: /version-cat-000/ })
    .getByRole("button", { name: "Restore" })
    .click();
  await expect
    .poll(() =>
      uploadMetrics.restoredVersions().some(
        (entry) =>
          entry.key === "gallery/cat.png" &&
          entry.versionId === "version-cat-000" &&
          entry.targetPath === "gallery/cat.png"
      )
    )
    .toBe(true);
  await expect(page.getByText('"target_path": "gallery/cat.png"')).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("switch", { name: "Show thumbnails" })).toBeChecked();
  await page.getByRole("button", { name: "Version history" }).click();
  await expect(page.getByRole("button", { name: "Thumbnail for gallery/cat.png" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Thumbnail for version version-cat-001" })).toBeVisible();
  await page.getByRole("button", { name: "Thumbnail for version version-cat-001" }).click();
  await expect(page.getByLabel("Media viewer thumbnails")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "gallery/cat.png version-cat-001", exact: true })
  ).toHaveAttribute("aria-current", "true");
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Next item" }).click();
  await expect(
    page.getByRole("button", { name: "gallery/cat.png version-cat-000", exact: true })
  ).toHaveAttribute("aria-current", "true");
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Thumbnail for gallery/cat.png" }).click();
  await expect(page.getByLabel("Media viewer thumbnails")).toBeVisible();
  await expect(page.getByRole("button", { name: "gallery/cat.png", exact: true })).toHaveAttribute(
    "aria-current",
    "true"
  );
  const mediaViewerDialog = page
    .getByRole("dialog")
    .filter({ has: page.getByLabel("Media viewer thumbnails") });
  await expect(mediaViewerDialog.getByRole("button", { name: "Version history" })).toBeVisible();
  await mediaViewerDialog.getByRole("button", { name: "Version history" }).click();
  await expect(page.getByLabel("Key")).toHaveValue("gallery/cat.png");
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Media viewer thumbnails")).toBeVisible();
  await expect(mediaViewerDialog.getByRole("button", { name: "Start slideshow" })).toBeVisible();
  const mediaViewerZoomSurface = page.locator('[data-media-zoom-surface="true"]').first();
  await expect(mediaViewerZoomSurface).toHaveAttribute("data-media-zoom-scale", "1.00");
  await dispatchCtrlWheel(mediaViewerZoomSurface, -240);
  await expect
    .poll(async () => Number(await mediaViewerZoomSurface.getAttribute("data-media-zoom-scale") ?? "1"))
    .toBeGreaterThan(1);
  const slideshowEntryScale = Number(
    (await mediaViewerZoomSurface.getAttribute("data-media-zoom-scale")) ?? "1"
  );
  await mediaViewerDialog.getByRole("button", { name: "Start slideshow" }).click();
  await expect(page.getByLabel("Media viewer thumbnails")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Next item" })).toHaveCount(0);
  await dispatchCtrlWheel(mediaViewerZoomSurface, -120);
  await expect
    .poll(async () => Number(await mediaViewerZoomSurface.getAttribute("data-media-zoom-scale") ?? "1"))
    .toBeGreaterThan(slideshowEntryScale);
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Media viewer thumbnails")).toBeVisible();
  await expect(mediaViewerDialog.getByRole("button", { name: "Start slideshow" })).toBeVisible();
  await expect(page.getByRole("button", { name: "gallery/clip.mp4", exact: true })).toHaveAttribute(
    "aria-current",
    "true"
  );
  await page.keyboard.press("Escape");
  await page.getByRole("row", { name: /docs\/readme\.txt/ }).getByRole("button", { name: "Read" }).click();
  await expect(page.getByText("hello from the mocked store")).toBeVisible();
  const explorerDownload = page.waitForEvent("download");
  await page.getByRole("row", { name: /docs\/readme\.txt/ }).getByRole("button", { name: "Download" }).click();
  expect((await explorerDownload).suggestedFilename()).toBe("mock.bin");
  await explorerTable
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "docs/", exact: true }) })
    .getByRole("button", { name: "Open" })
    .click();
  await expect(page.getByRole("cell", { name: "readme.txt" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "nested/" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "docs/" })).toHaveCount(0);
  await expect(page.getByRole("cell", { name: "gallery/" })).toHaveCount(0);
  await page.getByLabel("New folder name").fill("scratch");
  await page.getByRole("button", { name: "New folder" }).click();
  await expect(page.getByRole("cell", { name: "scratch/" })).toBeVisible();
  await page.locator('[data-explorer-upload-input="true"]').setInputFiles([
    {
      name: "quick-a.bin",
      mimeType: "application/octet-stream",
      buffer: Buffer.alloc(24, 0x71)
    },
    {
      name: "quick-b.bin",
      mimeType: "application/octet-stream",
      buffer: Buffer.alloc(12, 0x72)
    }
  ]);
  await expect(page.getByRole("heading", { name: "Store" })).toBeVisible();
  await expect(page.getByText("docs/quick-a.bin", { exact: true })).toBeVisible();
  await expect(page.getByText("docs/quick-b.bin", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Uploads 4/5 · 1 canceled" })).toBeVisible();
  await page.getByText("Explorer", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Explorer" })).toBeVisible();
  await page.getByRole("row", { name: /docs\/\s+prefix/i }).getByRole("button", { name: "Open" }).click();
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("row", { name: /scratch\/\s+prefix/i }).getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("cell", { name: "scratch/" })).toHaveCount(0);
  page.once("dialog", (dialog) => dialog.accept("docs/nested/quick-c.bin"));
  await page.getByRole("row", { name: /quick-b\.bin/ }).getByRole("button", { name: "Rename" }).click();
  await expect(page.getByRole("cell", { name: "nested/quick-c.bin" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "quick-b.bin" })).toHaveCount(0);
  await page.getByRole("button", { name: "Version history" }).click();
  await page.getByLabel("Key").fill("docs/readme.txt");
  await page.getByRole("button", { name: "Load versions" }).click();
  await expect(page.getByRole("cell", { name: "version-001", exact: true })).toBeVisible();
  await page.keyboard.press("Escape");

  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  await expect(page.getByText("gallery/clip.mp4", { exact: true })).toBeVisible();
  await expect(page.getByText("3 items")).toBeVisible();
  await expect(page.getByText("1 movie")).toBeVisible();
  const thumbnailsPerRowInput = page.getByLabel("Thumbnails per row");
  await thumbnailsPerRowInput.fill("8");
  await thumbnailsPerRowInput.blur();
  await expect(thumbnailsPerRowInput).toHaveValue("8");
  await page.getByText("cat.png", { exact: true }).click();
  const galleryDialog = page
    .getByRole("dialog")
    .filter({ has: page.getByLabel("Media viewer thumbnails") });
  await expect(galleryDialog.getByRole("button", { name: "Version history" })).toBeVisible();
  await galleryDialog.getByRole("button", { name: "Version history" }).click();
  await expect(page.getByLabel("Key")).toHaveValue("gallery/cat.png");
  page.once("dialog", (dialog) => {
    void dialog.accept("gallery/restored-cat-from-gallery.png");
  });
  await page
    .getByRole("row", { name: /version-cat-000/ })
    .getByRole("button", { name: "Restore" })
    .click();
  await expect
    .poll(() =>
      uploadMetrics.restoredVersions().some(
        (entry) =>
          entry.key === "gallery/cat.png" &&
          entry.versionId === "version-cat-000" &&
          entry.targetPath === "gallery/restored-cat-from-gallery.png"
      )
    )
    .toBe(true);
  await expect(page.getByText('Restored version "version-cat-000" to "gallery/restored-cat-from-gallery.png".')).toBeVisible();
  const galleryVersionThumbnail = page.getByRole("button", {
    name: "Thumbnail for version version-cat-001"
  });
  await expect(galleryVersionThumbnail).toBeVisible();
  await galleryVersionThumbnail.click();
  await page.keyboard.press("Escape");
  await expect(
    page.getByRole("button", { name: "gallery/cat.png version-cat-001", exact: true })
  ).toHaveAttribute("aria-current", "true");
  await page.keyboard.press("Escape");
  await page.getByText("clip.mp4", { exact: true }).click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.locator("video")).toBeVisible();
  await page.keyboard.press("Escape");
  await page.getByText("Cluster", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Cluster" })).toBeVisible();
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByLabel("Thumbnails per row")).toHaveValue("8");
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByLabel("Thumbnails per row")).toHaveValue("8");
  await page.getByRole("button", { name: "Map" }).click();
  await expect(
    page.getByText("Using Natural Earth Globe from your self-hosted basemap dataset.")
  ).toBeVisible();
  await expect(page.getByText("Self-hosted basemap unavailable")).toHaveCount(0);
  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect(page.getByText("2 markers")).toBeVisible();
  await page.getByRole("button", { name: "Fullscreen map" }).click();
  await expect(page.getByRole("button", { name: "Exit fullscreen map" })).toHaveCount(0);
  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  expect(
    await page
      .locator('[aria-label="Geotagged gallery map"]')
      .evaluate((element) => element.parentElement?.tagName)
  ).toBe("BODY");
  const mapMarkerButton = page.getByRole("button", { name: "Open map marker for gallery/cat.png" });
  await expect(mapMarkerButton).toBeVisible();
  expect(
    await mapMarkerButton.evaluate(
      (element) => element.closest(".maplibregl-canvas-container") !== null
    )
  ).toBe(true);
  await mapMarkerButton.click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.getByText("Loading original image")).toBeVisible();
  await expect(page.getByText("Loading original image")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "gallery/cat.png", exact: true })).toHaveAttribute(
    "aria-current",
    "true"
  );
  await page.getByRole("button", { name: "Next item" }).click();
  await expect(page.getByRole("button", { name: "gallery/dog.jpg", exact: true })).toHaveAttribute(
    "aria-current",
    "true"
  );
  await expect(page.getByRole("button", { name: "gallery/clip.mp4", exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Next item" })).toBeDisabled();
  await page.keyboard.press("Escape");
  await page.goBack();
  await expect(page.getByRole("button", { name: "Fullscreen map" })).toBeVisible();
  const prefixInput = page.getByLabel("Prefix");
  await expect(page.getByRole("button", { name: "nested/", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "docs/", exact: true }).click();
  await expect(prefixInput).toHaveValue("docs/");
  await expect(page.getByRole("button", { name: "nested/", exact: true })).toBeVisible();
  await expect(page.getByText("No geo-tagged media in view")).toBeVisible();
  await page.getByRole("button", { name: "nested/", exact: true }).click();
  await expect(prefixInput).toHaveValue("docs/nested/");
  await page.getByRole("button", { name: "Up one level" }).click();
  await expect(prefixInput).toHaveValue("docs/");
  await page.getByRole("button", { name: "Up one level" }).click();
  await expect(prefixInput).toHaveValue("");
  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect(page.getByText("2 markers")).toBeVisible();
  await page.getByRole("button", { name: "media/", exact: true }).click();
  await expect(prefixInput).toHaveValue("media/");
  await expect(page.getByText("No geo-tagged media in view")).toBeVisible();
  await page.getByRole("button", { name: "Up one level" }).click();
  await expect(prefixInput).toHaveValue("");
  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect(pageErrors).toEqual([]);
  await page.getByRole("button", { name: "Grid" }).click();
  await page.getByLabel("Prefix").fill("docs/");
  await page.getByRole("button", { name: "Load" }).click();
  await expect(page.getByText("nested/", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("Up one level")).toBeVisible();
  await expect(page.getByText("No media objects in view")).toHaveCount(0);
  await page.getByText("Up one level").click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();

  await page.getByText("Cluster", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Cluster" })).toBeVisible();
  await expect(page.getByText("Under replicated")).toBeVisible();
  await expect(page.locator("pre").filter({ hasText: '"node_id": "node-alpha"' })).toBeVisible();
  await expect(page.getByText('"under_replicated": 1')).toBeVisible();

  const requestedPaths = uploadMetrics.requestedPaths();
  expect(requestedPaths).toEqual(
    expect.arrayContaining([
      apiV1("/ping"),
      apiV1("/health"),
      apiV1("/cluster/status"),
      apiV1("/connection-routes"),
      apiV1("/logs"),
      apiV1("/diagnostics/log-export"),
      apiV1("/rendezvous"),
      apiV1("/store/list"),
      apiV1("/store/uploads/start")
    ])
  );
  expect(requestedPaths.some((path) => path.startsWith(apiV1("/store/stream-binary")))).toBe(true);
  expect(requestedPaths.some((path) => path.startsWith(apiV1("/maps/")))).toBe(true);
  expect(requestedPaths).not.toContain("/api/ping");
  expect(requestedPaths).not.toContain("/api/health");
  expect(requestedPaths).not.toContain("/api/cluster/status");
  expect(requestedPaths).not.toContain("/api/store/list");
  expect(requestedPaths).not.toContain("/api/maps/logical-file");
});

test("client-ui gallery loads bounded server-side map clusters", async ({ page }) => {
  const mapClusterRequests: URL[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === apiV1("/gallery/map/clusters")) {
      mapClusterRequests.push(url);
    }
  });

  await installClientUiMocks(page, {
    storeEntries: createGalleryPaginationMockStoreEntries(520)
  });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
  await page.getByRole("button", { name: "Map" }).click();

  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect(page.getByText("523 items", { exact: true })).toBeVisible();
  await expect(page.getByText("522 photos", { exact: true })).toBeVisible();
  await expect(page.getByText("1 movie", { exact: true })).toBeVisible();
  await expect(page.getByText("522 ready", { exact: true })).toBeVisible();
  await expect(page.getByText("1 pending", { exact: true })).toBeVisible();
  await expect(page.getByText("2 markers", { exact: true })).toBeVisible();
  await expect(page.getByText("521 without GPS", { exact: true })).toBeVisible();
  await expect.poll(() => mapClusterRequests.length).toBeGreaterThan(0);
  expect(mapClusterRequests.every((request) => request.searchParams.get("depth") === "64")).toBe(
    true
  );
  expect(
    mapClusterRequests.every(
      (request) =>
        request.searchParams.has("south") &&
        request.searchParams.has("west") &&
        request.searchParams.has("north") &&
        request.searchParams.has("east") &&
        request.searchParams.has("zoom")
    )
  ).toBe(true);
});

test("client-ui gallery falls back to an older map API", async ({ page }) => {
  const mapRequestPaths: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.includes("/map/clusters")) {
      mapRequestPaths.push(url.pathname);
    }
  });
  await installClientUiMocks(page, { legacyGalleryMapApiOnly: true });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await page.getByRole("button", { name: "Map" }).click();

  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect.poll(() => mapRequestPaths).toContain(apiV1("/gallery/map/clusters"));
  await expect.poll(() => mapRequestPaths).toContain(apiV1("/store/map/clusters"));
});

test("client-ui keeps an open server cluster stable across viewport refreshes", async ({ page }) => {
  const storeEntries = createMockStoreEntries().map((entry) =>
    entry.path === "gallery/dog.jpg" && entry.media
      ? {
          ...entry,
          media: {
            ...entry.media,
            gps: {
              latitude: 47.3769,
              longitude: 8.5417
            }
          }
        }
      : entry
  );
  const mapClusterRequests: URL[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === apiV1("/gallery/map/clusters")) {
      mapClusterRequests.push(url);
    }
  });

  await installClientUiMocks(page, {
    storeEntries,
    mapClusterRefreshDelayMs: 750,
    mapClusterEntriesDelayMs: 1_500
  });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await page.getByRole("button", { name: "Map" }).click();

  const map = page.locator('[aria-label="Geotagged gallery map"]');
  const cluster = page.getByRole("button", { name: "Open map cluster with 2 items" });
  await expect(cluster).toBeVisible();
  await map.hover({ position: { x: 80, y: 80 } });
  await page.mouse.wheel(0, -400);
  await expect.poll(() => mapClusterRequests.length).toBeGreaterThan(1);
  await cluster.click();

  const chooser = page.getByRole("dialog", { name: "2 items in map cluster" });
  await expect(chooser).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/cat.png" })).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/dog.jpg" })).toBeVisible();
});

test("client-ui keeps the direct iOS gallery map inside the WebView viewport", async ({ page }) => {
  await installClientUiMocks(page);
  await page.goto("/?embedded=gallery_map&embedded_client=ios");

  const fullscreenButton = page.getByRole("button", { name: "Fullscreen map" });
  await expect(fullscreenButton).toBeVisible();
  await fullscreenButton.click();

  const fullscreenMap = page.getByLabel("Geotagged gallery map");
  await expect
    .poll(() =>
      fullscreenMap.evaluate((element) => element.getBoundingClientRect().height / window.innerHeight)
    )
    .toBeGreaterThan(0.9);
});

test("client-ui synchronizes direct Android gallery fullscreen with its native host", async ({ page }) => {
  await page.addInitScript(() => {
    const messages: string[] = [];
    (window as Window & {
      IronmeshAndroidUi?: { postMessage: (message: string) => void };
      ironmeshAndroidFullscreenMessages?: string[];
    }).IronmeshAndroidUi = {
      postMessage(message) {
        messages.push(message);
      }
    };
    (window as Window & { ironmeshAndroidFullscreenMessages?: string[] })
      .ironmeshAndroidFullscreenMessages = messages;
  });
  await installClientUiMocks(page);
  await page.goto("/?embedded=gallery_map&embedded_client=android");

  await page.getByRole("button", { name: "Fullscreen map" }).click();

  const fullscreenMap = page.getByLabel("Geotagged gallery map");
  await expect
    .poll(() =>
      fullscreenMap.evaluate((element) => element.getBoundingClientRect().height / window.innerHeight)
    )
    .toBeGreaterThan(0.9);

  const exitControl = page.locator('[data-gallery-map-fullscreen-exit="true"]');
  await expect(exitControl).toBeVisible();
  expect(
    await exitControl.evaluate((element) => {
      const style = getComputedStyle(element);
      return style.position === "fixed" && Number(style.zIndex) > 150;
    })
  ).toBe(true);
  await expect
    .poll(() =>
      page.evaluate(() => {
        const messages = (window as Window & { ironmeshAndroidFullscreenMessages?: string[] })
          .ironmeshAndroidFullscreenMessages ?? [];
        return JSON.parse(messages[messages.length - 1] ?? "{}").fullscreen;
      })
    )
    .toBe(true);

  await page.evaluate(() => {
    window.dispatchEvent(new Event("ironmesh:gallery-map-exit-fullscreen"));
  });
  await expect(exitControl).toHaveCount(0);
  await expect
    .poll(() =>
      page.evaluate(() => {
        const messages = (window as Window & { ironmeshAndroidFullscreenMessages?: string[] })
          .ironmeshAndroidFullscreenMessages ?? [];
        return JSON.parse(messages[messages.length - 1] ?? "{}").fullscreen;
      })
    )
    .toBe(false);

  await page.getByRole("button", { name: "Fullscreen map" }).click();
  await expect(exitControl).toBeVisible();
  await exitControl.getByRole("button", { name: "Exit fullscreen map" }).click();
  await expect(exitControl).toHaveCount(0);
  await expect
    .poll(() =>
      page.evaluate(() => {
        const messages = (window as Window & { ironmeshAndroidFullscreenMessages?: string[] })
          .ironmeshAndroidFullscreenMessages ?? [];
        return JSON.parse(messages[messages.length - 1] ?? "{}").fullscreen;
      })
    )
    .toBe(false);
});

for (const embeddedClient of [null] as const) {
  const clientLabel = embeddedClient ?? "browser";
  test(`client-ui ${clientLabel} media viewer downloads the original`, async ({ page }) => {
    await installClientUiMocks(page);
    if (embeddedClient) {
      await page.setViewportSize({ width: 390, height: 844 });
    }
    const query = new URLSearchParams({ page: "gallery" });
    if (embeddedClient) {
      query.set("embedded_client", embeddedClient);
    }
    await page.goto(`/?${query.toString()}`);
    await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
    await page.getByText("gallery/cat.png", { exact: true }).click();

    const dialog = page.getByRole("dialog");
    const mediaActions = dialog.locator('[data-media-actions="true"]');
    await expect(dialog.getByRole("button", { name: "Start slideshow" })).toBeVisible();
    await expect(mediaActions).toBeVisible();
    expect(await mediaActions.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
    const mediaActionsBox = await mediaActions.boundingBox();
    expect(mediaActionsBox).not.toBeNull();
    expect((mediaActionsBox?.x ?? -1) + (mediaActionsBox?.width ?? 0)).toBeLessThanOrEqual(
      page.viewportSize()?.width ?? Number.POSITIVE_INFINITY
    );
    const downloadPromise = page.waitForEvent("download");
    await dialog.getByRole("button", { name: "Download original" }).click();

    const download = await downloadPromise;
    expect(download.suggestedFilename()).toBe("cat.png");
    await expect(dialog).toBeVisible();
  });
}

test("client-ui Android media viewer shares an immutable original through the native bridge", async ({
  page
}) => {
  await installAndroidShareBridgeMock(page);
  await installClientUiMocks(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/?page=gallery&embedded_client=android");
  await page.getByRole("textbox", { name: "Snapshot" }).click();
  await page.getByRole("option", { name: "snapshot-001" }).click();
  await page.getByText("gallery/cat.png", { exact: true }).click();

  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "Download original" })).toHaveCount(0);
  await dialog.getByRole("button", { name: "Share original" }).click();
  await expect(dialog.getByRole("button", { name: "Share opened" })).toBeVisible();

  const payloads = await androidShareMessages(page);
  expect(payloads).toHaveLength(1);
  expect(JSON.parse(payloads[0])).toMatchObject({
    action: "share-original",
    key: "gallery/cat.png",
    versionId: null,
    snapshotId: "snapshot-001",
    fileName: "cat.png",
    mimeType: "image/png",
    sizeBytes: 3_145_728
  });
});

test("client-ui iOS media viewer shares an immutable original through the native bridge", async ({
  page
}) => {
  await page.addInitScript(() => {
    const messages: Array<Record<string, unknown>> = [];
    Object.assign(window, {
      __ironmeshIosShareMessages: messages,
      webkit: {
        messageHandlers: {
          IronmeshIosShare: {
            postMessage(message: Record<string, unknown>) {
              messages.push(message);
              return Promise.resolve({ requestId: message.requestId, status: "opened" });
            }
          }
        }
      }
    });
  });
  await installClientUiMocks(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/?page=gallery&embedded_client=ios");
  await page.getByRole("textbox", { name: "Snapshot" }).click();
  await page.getByRole("option", { name: "snapshot-001" }).click();
  await page.getByText("gallery/cat.png", { exact: true }).click();

  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "Download original" })).toHaveCount(0);
  await dialog.getByRole("button", { name: "Share original" }).click();
  await expect(dialog.getByRole("button", { name: "Share opened" })).toBeVisible();

  const payloads = await page.evaluate(
    () =>
      (window as typeof window & { __ironmeshIosShareMessages: Array<Record<string, unknown>> })
        .__ironmeshIosShareMessages
  );
  expect(payloads).toHaveLength(1);
  expect(payloads[0]).toMatchObject({
    action: "share-original",
    key: "gallery/cat.png",
    versionId: null,
    snapshotId: "snapshot-001",
    fileName: "cat.png",
    mimeType: "image/png",
    sizeBytes: 3_145_728
  });
});

test("client-ui Android explorer resolves the preferred current version before sharing", async ({
  page
}) => {
  await installAndroidShareBridgeMock(page);
  const mocks = await installClientUiMocks(page, {
    storeEntries: createMockStoreEntries().map((entry) =>
      entry.path === "gallery/cat.png" ? { ...entry, version: undefined } : entry
    )
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/?page=explorer&embedded_client=android");
  await page.getByRole("button", { name: "Thumbnail for gallery/cat.png" }).click();

  const dialog = page.getByRole("dialog");
  const shareButton = dialog.getByRole("button", { name: "Share original" });
  await expect(shareButton).toBeEnabled();
  expect(mocks.requestedPaths()).not.toContain(apiV1("/versions"));
  await shareButton.click();
  await expect(dialog.getByRole("button", { name: "Share opened" })).toBeVisible();
  expect(mocks.requestedPaths()).toContain(apiV1("/versions"));

  const payloads = await androidShareMessages(page);
  expect(payloads).toHaveLength(1);
  expect(JSON.parse(payloads[0])).toMatchObject({
    action: "share-original",
    key: "gallery/cat.png",
    versionId: "version-cat-001",
    snapshotId: null,
    fileName: "cat.png",
    mimeType: "image/png",
    sizeBytes: 3_145_728
  });
});

test("client-ui iOS media viewer ignores stale native share responses", async ({ page }) => {
  await page.addInitScript(() => {
    const pending: Array<{
      key: string;
      requestId: string;
      resolve: (response: { requestId: string; status: "opened" | "error" }) => void;
    }> = [];
    Object.assign(window, {
      __ironmeshIosPendingShares: pending,
      __resolveIronmeshIosShare(index: number, status: "opened" | "error") {
        const entry = pending[index];
        entry?.resolve({ requestId: entry.requestId, status });
      },
      webkit: {
        messageHandlers: {
          IronmeshIosShare: {
            postMessage(message: Record<string, unknown>) {
              return new Promise((resolve) => {
                pending.push({
                  key: String(message.key ?? ""),
                  requestId: String(message.requestId ?? ""),
                  resolve
                });
              });
            }
          }
        }
      }
    });
  });
  await installClientUiMocks(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/?page=gallery&embedded_client=ios");
  await page.getByRole("textbox", { name: "Snapshot" }).click();
  await page.getByRole("option", { name: "snapshot-001" }).click();
  await page.getByText("gallery/cat.png", { exact: true }).click();

  const dialog = page.getByRole("dialog");
  await dialog.getByRole("button", { name: "Share original" }).click();
  await expect(dialog.getByRole("button", { name: "Preparing share…" })).toBeVisible();
  await dialog.getByRole("button", { name: "Next item" }).click();
  await expect(dialog).toHaveAccessibleName(/dog\.jpg/);
  await dialog.getByRole("button", { name: "Share original" }).click();
  await expect(dialog.getByRole("button", { name: "Preparing share…" })).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (
            window as typeof window & {
              __ironmeshIosPendingShares: Array<unknown>;
            }
          ).__ironmeshIosPendingShares.length
      )
    )
    .toBe(2);
  expect(
    await page.evaluate(
      () =>
        (
          window as typeof window & {
            __ironmeshIosPendingShares: Array<{ key: string }>;
          }
        ).__ironmeshIosPendingShares.map((entry) => entry.key)
    )
  ).toEqual(["gallery/cat.png", "gallery/dog.jpg"]);

  await page.evaluate(() => {
    (
      window as typeof window & {
        __resolveIronmeshIosShare: (index: number, status: "opened" | "error") => void;
      }
    ).__resolveIronmeshIosShare(0, "opened");
  });
  await expect(dialog.getByRole("button", { name: "Preparing share…" })).toBeVisible();

  await page.evaluate(() => {
    (
      window as typeof window & {
        __resolveIronmeshIosShare: (index: number, status: "opened" | "error") => void;
      }
    ).__resolveIronmeshIosShare(1, "opened");
  });
  await expect(dialog.getByRole("button", { name: "Share opened" })).toBeVisible();
});

test("client-ui gallery restores its persistent cache while the upstream is offline", async ({
  page
}) => {
  const mocks = await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  const initialRequestCount = mocks.galleryStoreListRequestCount();
  mocks.setGalleryOffline(true);
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();

  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  await expect
    .poll(() => mocks.galleryStoreListRequestCount())
    .toBeGreaterThan(initialRequestCount);
  await expect(page.getByText("mock gallery is offline")).toHaveCount(0);
});

test("client-ui gallery visibly replaces restored data after background revalidation", async ({
  page
}) => {
  const mocks = await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  const initialRequestCount = mocks.galleryStoreListRequestCount();
  expect(initialRequestCount).toBe(2);

  mocks.replaceStoreEntries(createRevalidatedGalleryMockStoreEntries());
  mocks.setGalleryStoreListDelay(400);
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();

  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  await expect(page.getByText("gallery/revalidated.png", { exact: true })).toBeVisible();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);
  expect(mocks.galleryStoreListRequestCount()).toBe(initialRequestCount + 2);
});

test("client-ui gallery background updates preserve current controls", async ({ page }) => {
  const mocks = await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();

  mocks.setGalleryStoreListDelayForMediaFilter("all", 2_000);
  const delayedAllResponse = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return url.pathname === apiV1("/store/list") && url.searchParams.get("media_filter") === "all";
  });
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();

  const mediaFilter = page.getByRole("textbox", { name: "Media" });
  await mediaFilter.click();
  await page.getByRole("option", { name: "Movies only", exact: true }).click();
  await expect(mediaFilter).toHaveValue("Movies only");
  const mediaGrid = page.locator('[data-gallery-grid="true"]');
  await expect(mediaGrid.getByText("gallery/clip.mp4", { exact: true })).toBeVisible();
  await expect(mediaGrid.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);

  await delayedAllResponse;
  await expect(mediaFilter).toHaveValue("Movies only");
  await expect(mediaGrid.getByText("gallery/clip.mp4", { exact: true })).toBeVisible();
  await expect(mediaGrid.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);
});

test("client-ui gallery cache is isolated when the authenticated cache scope changes", async ({
  page
}) => {
  const mocks = await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();

  mocks.setCacheScope("b".repeat(64));
  mocks.setGalleryOffline(true);
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();

  await expect(page.getByText("mock gallery is offline")).toBeVisible();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);
});

test("client-ui gallery never persists data without an authenticated cache scope", async ({
  page
}) => {
  const mocks = await installClientUiMocks(page, { cacheScope: null });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();
  expect(await galleryCacheDatabaseExists(page)).toBe(false);

  mocks.setGalleryOffline(true);
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();

  await expect(page.getByText("mock gallery is offline")).toBeVisible();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);
});

test("client-ui gallery ignores old-schema IndexedDB records and falls back safely", async ({
  page
}) => {
  const mocks = await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toBeVisible();

  await expireGalleryCacheSchema(page);
  mocks.setGalleryOffline(true);
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();

  await expect(page.getByText("mock gallery is offline")).toBeVisible();
  await expect(page.getByText("gallery/cat.png", { exact: true })).toHaveCount(0);
});

test("client-ui gallery cards stay compact on narrow viewports", async ({ page }) => {
  test.setTimeout(45_000);

  await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();

  await page.setViewportSize({ width: 390, height: 844 });

  const thumbnailsPerRowInput = page.getByLabel("Thumbnails per row");
  await thumbnailsPerRowInput.fill("8");
  await thumbnailsPerRowInput.blur();
  await expect(thumbnailsPerRowInput).toHaveValue("8");

  const galleryGrid = page
    .locator('[data-gallery-grid="true"]')
    .filter({ has: page.locator('[data-gallery-card="true"]') })
    .last();
  await expect(galleryGrid).toBeVisible();

  const gap = await galleryGrid.evaluate((node) => Number.parseFloat(getComputedStyle(node).gap));
  expect(gap).toBeLessThanOrEqual(10);

  const metadataToggle = page.getByLabel("Show metadata");
  await expect(metadataToggle).toBeChecked();
  await page.locator("label").filter({ hasText: "Show metadata" }).click();
  await expect(metadataToggle).not.toBeChecked();
  await expect(page.locator('[data-gallery-card-metadata="true"]')).toHaveCount(0);

  const collapsedGap = await galleryGrid.evaluate((node) =>
    Number.parseFloat(getComputedStyle(node).gap)
  );
  expect(collapsedGap).toBe(0);

  const previewHeightDelta = await page
    .locator('[data-gallery-card="true"]')
    .first()
    .evaluate((card) => {
      const aspectRatio = card.querySelector(".mantine-AspectRatio-root");
      if (!aspectRatio) {
        return null;
      }

      return Math.abs(
        card.getBoundingClientRect().height - aspectRatio.getBoundingClientRect().height
      );
    });
  expect(previewHeightDelta).not.toBeNull();
  expect(previewHeightDelta ?? Number.POSITIVE_INFINITY).toBeLessThanOrEqual(1);

  const borderWidth = await page
    .locator('[data-gallery-card="true"]')
    .first()
    .evaluate((node) => Number.parseFloat(getComputedStyle(node).borderTopWidth));
  expect(borderWidth).toBe(0);

  await page.setViewportSize({ width: 1280, height: 800 });
  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByLabel("Show metadata")).not.toBeChecked();
  await expect(page.locator('[data-gallery-card-metadata="true"]')).toHaveCount(0);
});

test("client-ui gallery recovers from a basemap metadata failure", async ({ page }) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => {
    pageErrors.push(error.message);
  });

  await installClientUiMocks(page, { mapMetadataStatus: 502 });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();

  await page.getByRole("button", { name: "Map" }).click();
  await expect(page.getByText("Self-hosted basemap unavailable")).toBeVisible();
  await expect(
    page.getByText("failed to load self-hosted basemap metadata: HTTP 502")
  ).toBeVisible();
  await expect(page.locator('[aria-label="Geotagged gallery map"]')).toBeVisible();
  await expect(page.getByRole("button", { name: "Switch to grid view" })).toBeVisible();
  expect(pageErrors).toEqual([]);

  await page.getByRole("button", { name: "Switch to grid view" }).click();
  await expect(page.getByRole("button", { name: "Grid" })).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator('[data-gallery-grid="true"]').last()).toBeVisible();
  expect(await page.evaluate(() => window.localStorage.getItem("ironmesh.gallery.view_mode"))).toBe(
    "grid"
  );

  await page.reload();
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("button", { name: "Grid" })).toHaveAttribute("aria-pressed", "true");
  expect(pageErrors).toEqual([]);
});

test("client-ui gallery exposes all sort orders", async ({ page }) => {
  await installClientUiMocks(page);
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();

  const gallerySort = page.getByRole("textbox", { name: "Sort" });
  await gallerySort.click();
  await expect(page.getByRole("option", { name: "Date (newest first)", exact: true })).toBeVisible();
  await expect(page.getByRole("option", { name: "Date (oldest first)", exact: true })).toBeVisible();
  await expect(page.getByRole("option", { name: "Path (A–Z)", exact: true })).toBeVisible();
  await expect(page.getByRole("option", { name: "Path (Z–A)", exact: true })).toBeVisible();

  await page.getByRole("option", { name: "Date (oldest first)", exact: true }).click();
  await expect(gallerySort).toHaveValue("Date (oldest first)");
});

test("client-ui gallery lightbox skips unsupported iOS originals and keeps the thumbnail fallback", async ({
  page
}) => {
  const mockState = await installClientUiMocks(page, {
    storeEntries: createHeicGalleryMockStoreEntries()
  });

  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
  await page.getByText("gallery/ios-photo.heic", { exact: true }).click();

  const galleryDialog = page.getByRole("dialog");
  await expect(galleryDialog).toBeVisible();
  await expect(page.getByText("Loading original image")).toHaveCount(0);
  await expect(page.getByText("Full image unavailable, showing thumbnail")).toHaveCount(0);
  await expect(
    page.getByText("Browser cannot preview the original format, showing thumbnail")
  ).toBeVisible();
  expect(mockState.requestedPaths()).not.toContain(apiV1("/store/stream-binary"));

  const downloadPromise = page.waitForEvent("download");
  await galleryDialog.getByRole("button", { name: "Download original" }).click();
  expect((await downloadPromise).suggestedFilename()).toBe("ios-photo.heic");
});

test("client-ui gallery and explorer lightboxes prefer the mobile viewer thumbnail on narrow touch viewports", async ({
  page
}) => {
  test.setTimeout(45_000);

  await page.addInitScript((forcedQuery) => {
    const originalMatchMedia = window.matchMedia.bind(window);
    window.matchMedia = (query: string) => {
      const result = originalMatchMedia(query);
      if (query !== forcedQuery) {
        return result;
      }

      return {
        matches: true,
        media: result.media,
        onchange: result.onchange,
        addListener: result.addListener?.bind(result) ?? (() => undefined),
        removeListener: result.removeListener?.bind(result) ?? (() => undefined),
        addEventListener: result.addEventListener.bind(result),
        removeEventListener: result.removeEventListener.bind(result),
        dispatchEvent: result.dispatchEvent.bind(result)
      } as MediaQueryList;
    };
  }, "(max-width: 48em) and (pointer: coarse)");

  await installClientUiMocks(page);

  const mediaRequests: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (
      url.pathname === apiV1("/media/thumbnail") ||
      url.pathname === "/media/thumbnail" ||
      url.pathname === apiV1("/store/stream-binary")
    ) {
      mediaRequests.push(`${url.pathname}${url.search}`);
    }
  });

  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();

  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByText("gallery/cat.png", { exact: true }).click();
  await expect(page.getByLabel("Media viewer thumbnails")).toBeVisible();

  await expect
    .poll(() =>
      mediaRequests.some(
        (requestPath) =>
          requestPath === `${apiV1("/media/thumbnail")}?key=gallery%2Fcat.png&profile=mobile_viewer` ||
          requestPath === `/media/thumbnail?key=gallery%2Fcat.png&profile=mobile_viewer`
      )
    )
    .toBe(true);

  await page.keyboard.press("Escape");
  await page.goto("/?page=explorer");
  await expect(page.getByRole("heading", { name: "Explorer" })).toBeVisible();
  await expect(page.getByRole("switch", { name: "Show thumbnails" })).toBeChecked();
  await page.getByRole("button", { name: "Thumbnail for gallery/cat.png" }).click();
  await expect(
    page.getByRole("dialog").locator('img[src*="profile=mobile_viewer"]').first()
  ).toBeVisible();
});

test("client-ui gallery virtual pages do not keep oversized spacer heights after sidebar resizing", async ({
  page
}) => {
  test.setTimeout(45_000);

  await installClientUiMocks(page, {
    storeEntries: createGalleryPaginationMockStoreEntries(72)
  });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();

  await page.setViewportSize({ width: 1280, height: 800 });

  const thumbnailsPerRowInput = page.getByLabel("Thumbnails per row");
  await thumbnailsPerRowInput.fill("8");
  await thumbnailsPerRowInput.blur();
  await expect(thumbnailsPerRowInput).toHaveValue("8");

  await expect
    .poll(() => maxGalleryVirtualPageGap(page), {
      message: "expected loaded gallery pages to collapse to their rendered grid height"
    })
    .toBeLessThanOrEqual(2);

  const desktopSidebarToggle = page.getByRole("button", { name: "Toggle navigation sidebar" });
  await desktopSidebarToggle.click();

  await expect
    .poll(() => maxGalleryVirtualPageGap(page), {
      message: "expected gallery page heights to re-measure after widening the content area"
    })
    .toBeLessThanOrEqual(2);

  await desktopSidebarToggle.click();

  await expect
    .poll(() => maxGalleryVirtualPageGap(page), {
      message: "expected gallery page heights to re-measure after restoring the sidebar"
    })
    .toBeLessThanOrEqual(2);
});

test("client-ui gallery reuses an evicted virtual page without requesting it again", async ({
  page
}) => {
  test.setTimeout(45_000);
  await page.addInitScript(() => {
    window.localStorage.setItem("ironmesh.gallery.thumbnails_per_row", "8");
    window.localStorage.setItem("ironmesh.gallery.show_metadata", "false");
  });

  const pageOffsets: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname !== apiV1("/store/list") || !url.searchParams.has("media_filter")) {
      return;
    }
    pageOffsets.push(url.searchParams.get("offset") ?? "0");
  });

  await installClientUiMocks(page, {
    storeEntries: createGalleryPaginationMockStoreEntries(500)
  });
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByAltText("gallery/paginated-001.jpg", { exact: true })).toBeVisible();

  const virtualPageSlots = page.locator('[data-gallery-virtual-page-slot="true"]');
  await expect(virtualPageSlots).toHaveCount(8);
  await virtualPageSlots.last().scrollIntoViewIfNeeded();
  await expect
    .poll(() => pageOffsets.some((offset) => Number(offset) > 0), {
      message: "expected the distant virtual gallery pages to load"
    })
    .toBe(true);

  const initialPageRequestCount = pageOffsets.filter((offset) => offset === "0").length;
  await virtualPageSlots.first().scrollIntoViewIfNeeded();
  await expect(page.getByAltText("gallery/paginated-001.jpg", { exact: true })).toBeVisible();
  await page.waitForTimeout(300);

  expect(pageOffsets.filter((offset) => offset === "0")).toHaveLength(initialPageRequestCount);
});

test("client-ui explorer fetches result pages instead of the complete index", async ({ page }) => {
  const requestPages: Array<{ offset: string | null; limit: string | null }> = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname !== apiV1("/store/list") || !url.searchParams.has("limit")) {
      return;
    }
    requestPages.push({
      offset: url.searchParams.get("offset"),
      limit: url.searchParams.get("limit")
    });
  });

  await installClientUiMocks(page, {
    storeEntries: createGalleryPaginationMockStoreEntries(250)
  });
  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await expect(page.locator('[data-explorer-pagination="true"]')).toContainText("Showing 1–100 of");

  const pagination = page.locator('[data-explorer-pagination="true"]');
  await pagination.getByRole("button", { name: "2", exact: true }).click();
  await expect
    .poll(() => requestPages.some((request) => request.offset === "100" && request.limit === "100"))
    .toBe(true);
  await expect(pagination).toContainText("Showing 101–200 of");
  expect(requestPages.every((request) => request.limit === "100")).toBe(true);
});

test("client-ui explorer keeps loaded history while paging current entries", async ({ page }) => {
  let historyRequestCount = 0;
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === apiV1("/store/history") && request.method() === "GET") {
      historyRequestCount += 1;
    }
  });

  await installClientUiMocks(page, {
    storeEntries: createGalleryPaginationMockStoreEntries(250),
    historyEntries: [
      {
        path: "deleted.txt",
        entry_type: "historical",
        restore_source_path: "deleted.txt",
        restore_version_id: "version-deleted-001",
        removed_at_unix: 1_712_345_600
      }
    ]
  });
  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await page.getByText("Show deleted or moved files", { exact: true }).click();
  await expect(page.getByRole("cell", { name: "deleted.txt", exact: true })).toBeVisible();
  await expect.poll(() => historyRequestCount).toBe(1);

  await page.locator('[data-explorer-pagination="true"]').getByRole("button", { name: "2" }).click();
  await expect(page.locator('[data-explorer-pagination="true"]')).toContainText("Showing 101–200 of");
  await expect(page.getByRole("cell", { name: "deleted.txt", exact: true })).toBeVisible();
  await page.waitForTimeout(300);
  expect(historyRequestCount).toBe(1);
});

test("client-ui explorer only caps depth for historical entries", async ({ page }) => {
  const currentDepths: string[] = [];
  const historyDepths: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === apiV1("/store/list")) {
      currentDepths.push(url.searchParams.get("depth") ?? "");
    }
    if (url.pathname === apiV1("/store/history")) {
      historyDepths.push(url.searchParams.get("depth") ?? "");
    }
  });

  await installClientUiMocks(page, {
    historyEntries: [
      {
        path: "deleted.txt",
        entry_type: "historical",
        restore_source_path: "deleted.txt",
        restore_version_id: "version-deleted",
        removed_at_unix: 1_712_345_600
      }
    ]
  });
  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await page.getByText("Show deleted or moved files", { exact: true }).click();
  await page.getByLabel("Depth").fill("65");
  await page.getByRole("button", { name: "Refresh entries" }).click();

  await expect.poll(() => currentDepths.includes("65")).toBe(true);
  await expect.poll(() => historyDepths.includes("64")).toBe(true);
});

test("client-ui explorer restores selected deleted and moved entries in one batch", async ({
  page
}) => {
  const mockState = await installClientUiMocks(page, {
    historyEntries: [
      {
        path: "deleted.txt",
        entry_type: "historical",
        restore_source_path: "deleted.txt",
        restore_version_id: "version-deleted-001",
        removed_at_unix: 1_712_345_600,
        moved_to_path: null
      },
      {
        path: "old-name.txt",
        entry_type: "historical",
        restore_source_path: "old-name.txt",
        restore_version_id: "version-moved-001",
        removed_at_unix: 1_712_345_601,
        moved_to_path: "new-name.txt"
      }
    ]
  });

  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Explorer" })).toBeVisible();
  await page.getByText("Show deleted or moved files", { exact: true }).click();
  await expect(page.getByRole("cell", { name: "deleted.txt", exact: true })).toBeVisible();
  await expect(page.getByRole("row", { name: /old-name\.txt/ })).toContainText(
    "moved to new-name.txt"
  );

  await page.getByLabel("Select historical entry deleted.txt").check();
  await page.getByLabel("Select historical entry old-name.txt").check();
  await page.getByRole("button", { name: "Restore selected" }).click();

  await expect
    .poll(() =>
      mockState
        .restoredHistoryEntries()
        .flat()
        .map((entry) => entry.path)
        .sort()
    )
    .toEqual(["deleted.txt", "old-name.txt"]);
  await expect(page.getByText('"requested_count": 2')).toBeVisible();

  await page.getByRole("textbox", { name: "Snapshot" }).click();
  await page.getByRole("option", { name: "snapshot-001" }).click();
  await page.getByRole("button", { name: "Load entries" }).click();
  await expect(page.getByText("Recoverable deleted and moved files")).toBeHidden();
});

test("client-ui explorer splits historical restores into supported batch sizes", async ({ page }) => {
  const mockState = await installClientUiMocks(page, {
    historyEntries: Array.from({ length: 101 }, (_, index) => {
      const path = `deleted-${String(index + 1).padStart(3, "0")}.txt`;
      return {
        path,
        entry_type: "historical",
        restore_source_path: path,
        restore_version_id: `version-deleted-${String(index + 1).padStart(3, "0")}`,
        removed_at_unix: 1_712_345_600 + index
      };
    })
  });

  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await page.getByText("Show deleted or moved files", { exact: true }).click();
  await expect(page.getByRole("cell", { name: "deleted-001.txt", exact: true })).toBeVisible();

  await page.getByLabel("Select all historical entries").check();
  await expect(page.getByText("101 historical items selected", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Restore selected" }).click();

  await expect
    .poll(() => mockState.restoredHistoryEntries().map((entries) => entries.length))
    .toEqual([100, 1]);
});

test("client-ui explorer retains completed historical restore batches after a later request fails", async ({
  page
}) => {
  const mockState = await installClientUiMocks(page, {
    historyRestoreFailureAtCall: 2,
    historyEntries: Array.from({ length: 101 }, (_, index) => {
      const path = `deleted-${String(index + 1).padStart(3, "0")}.txt`;
      return {
        path,
        entry_type: "historical",
        restore_source_path: path,
        restore_version_id: `version-deleted-${String(index + 1).padStart(3, "0")}`,
        removed_at_unix: 1_712_345_600 + index
      };
    })
  });

  await page.goto("/");
  await page.getByText("Explorer", { exact: true }).click();
  await page.getByText("Show deleted or moved files", { exact: true }).click();
  await expect(page.getByRole("cell", { name: "deleted-001.txt", exact: true })).toBeVisible();

  await page.getByLabel("Select all historical entries").check();
  await page.getByRole("button", { name: "Restore selected" }).click();

  await expect
    .poll(() => mockState.restoredHistoryEntries().map((entries) => entries.length))
    .toEqual([100]);
  await expect(page.getByRole("cell", { name: "deleted-001.txt", exact: true })).toBeHidden();
  await expect(page.getByRole("cell", { name: "deleted-101.txt", exact: true })).toBeVisible();
  await expect(page.getByLabel("Select historical entry deleted-101.txt")).toBeChecked();
  await expect(page.getByText(/restored before a later batch failed/)).toBeVisible();
});

test("client-ui desktop navigation can collapse and scroll on short viewports", async ({ page }) => {
  test.setTimeout(45_000);

  await installClientUiMocks(page);
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();

  const desktopSidebarToggle = page.getByRole("button", { name: "Toggle navigation sidebar" });
  const primaryNavigation = page.getByLabel("Primary navigation");
  await expect(desktopSidebarToggle).toBeVisible();
  await expect(primaryNavigation).toBeVisible();

  await page.setViewportSize({ width: 1280, height: 320 });
  const navbarScrollViewport = page.locator(".shell-navbar .mantine-ScrollArea-viewport");
  await expect(navbarScrollViewport).toBeVisible();
  const navbarScrollTop = await navbarScrollViewport.evaluate((node) => {
    node.scrollTop = 999;
    return node.scrollTop;
  });
  expect(navbarScrollTop).toBeGreaterThan(0);
  const navbarRightBeforeCollapse = await primaryNavigation.evaluate(
    (node) => node.getBoundingClientRect().right
  );
  expect(navbarRightBeforeCollapse).toBeGreaterThan(0);

  await desktopSidebarToggle.click();
  await expect
    .poll(async () => primaryNavigation.evaluate((node) => node.getBoundingClientRect().right))
    .toBeLessThanOrEqual(0);
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();

  await desktopSidebarToggle.click();
  await expect
    .poll(async () => primaryNavigation.evaluate((node) => node.getBoundingClientRect().right))
    .toBeGreaterThan(0);
  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
});

test("client-ui mobile drawer reveals and navigates its menu items", async ({ page }) => {
  await installClientUiMocks(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");

  const mobileMenuToggle = page.getByRole("button", { name: "Toggle navigation menu" });
  const primaryNavigation = page.getByLabel("Primary navigation");

  await expect(mobileMenuToggle).toBeVisible();
  await expect
    .poll(async () => primaryNavigation.evaluate((node) => node.getBoundingClientRect().right))
    .toBeLessThanOrEqual(0);

  await mobileMenuToggle.click();

  await expect
    .poll(async () => primaryNavigation.evaluate((node) => node.getBoundingClientRect().right))
    .toBeGreaterThan(0);
  await expect
    .poll(async () =>
      primaryNavigation.evaluate((node) => {
        const bounds = node.getBoundingClientRect();
        return bounds.height / window.innerHeight;
      })
    )
    .toBeGreaterThan(0.5);
  await expect(primaryNavigation).toBeVisible();
  await expect(primaryNavigation.getByText("Overview", { exact: true })).toBeVisible();
  await expect(primaryNavigation.getByText("Connection paths", { exact: true })).toBeVisible();
  await expect(primaryNavigation.getByText("Logs", { exact: true })).toBeVisible();

  await primaryNavigation.getByText("Logs", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Logs" })).toBeVisible();
  await expect
    .poll(async () => primaryNavigation.evaluate((node) => node.getBoundingClientRect().right))
    .toBeLessThanOrEqual(0);
});

type InstallClientUiMocksOptions = {
  storeEntries?: MockStoreEntry[];
  historyEntries?: MockHistoryEntry[];
  historyRestoreFailureAtCall?: number;
  cacheScope?: string | null;
  mapMetadataStatus?: number;
  mapMetadataCenter?: [number, number, number];
  mapConfigurationStatus?: number;
  mapConfiguration?: MockGalleryMapConfiguration;
  mapClusterRefreshDelayMs?: number;
  mapClusterEntriesDelayMs?: number;
  legacyGalleryMapApiOnly?: boolean;
};

type MockHistoryEntry = {
  path: string;
  entry_type: "historical";
  restore_source_path: string;
  restore_version_id: string;
  removed_at_unix: number;
  moved_to_path?: string | null;
};

type MockGalleryMapConfiguration = {
  active_variant_id: string;
  variants: Array<Record<string, unknown>>;
};

function defaultGalleryMapConfiguration(): MockGalleryMapConfiguration {
  return {
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
        raster_manifest_key: "sys/maps/natural-earth-globe.mbtiles.manifest.json"
      }
    ]
  };
}

async function installClientUiMocks(page: Page, options?: InstallClientUiMocksOptions) {
  const imageBody = tinyPngBuffer();
  const movieBody = Buffer.from("mock-movie-payload");
  const logicalMapBody = readFileSync("tests/fixtures/smoke.mbtiles");
  const emptyVectorTileBody = gzipSync(Buffer.alloc(0));
  const glyphRangeBody = Buffer.alloc(0);
  let uploadSessionStartCount = 0;
  const uploadSizes = new Map<string, number>();
  const uploadKeys = new Map<string, string>();
  let maxConcurrentUploadIds = 0;
  const activeUploadIds = new Set<string>();
  const deletedUploadSessionIds = new Set<string>();
  const requestedPaths = new Set<string>();
  const diagnosticContexts = new Set<string>();
  let diagnosticContextRequestCount = 0;
  const storeEntries = options?.storeEntries ?? createMockStoreEntries();
  let historyEntries = options?.historyEntries?.slice() ?? [];
  let cacheScope = options?.cacheScope === undefined ? "a".repeat(64) : options.cacheScope;
  let galleryOffline = false;
  let galleryStoreListRequestCount = 0;
  let galleryMapClusterRequestCount = 0;
  const galleryMapMock = new GalleryMapMockSession<MockStoreEntry>();
  let galleryStoreListDelayMs = 0;
  const galleryStoreListDelayByMediaFilter = new Map<string, number>();
  const restoredVersions: Array<{ key: string; versionId: string; targetPath: string }> = [];
  const restoredHistoryEntries: MockHistoryEntry[][] = [];
  let historyRestoreRequestCount = 0;
  const currentVersionByKey = new Map<string, string>([["gallery/cat.png", "version-cat-001"]]);
  const connectionRoutesPayload = {
    generated_at_unix_ms: 1_712_345_600_000,
    ranked_indices: [0, 2, 3, 1],
    endpoints: [
      {
        index: 0,
        path_kind: "direct_https",
        locator: "https://node-alpha.local",
        bootstrap_rank: 0,
        target_node_id: "node-alpha",
        score: 18.2,
        ewma_latency_ms: 18.2,
        ewma_throughput_bytes_per_sec: 225000,
        consecutive_failures: 0,
        total_failures: 0,
        total_successes: 12,
        last_measurement_unix_ms: 1_712_345_600_000,
        last_success_unix_ms: 1_712_345_600_000,
        last_used_unix_ms: 1_712_345_600_000,
        last_failure_unix_ms: null,
        circuit_open_until_unix_ms: null,
        background_probe_in_flight: false,
        last_background_probe_unix_ms: 1_712_345_580_000,
        last_error: null
      },
      {
        index: 1,
        path_kind: "direct_https",
        locator: "https://node-beta.local",
        bootstrap_rank: 1,
        target_node_id: "node-beta",
        score: 604.4,
        ewma_latency_ms: 104.4,
        ewma_throughput_bytes_per_sec: null,
        consecutive_failures: 2,
        total_failures: 3,
        total_successes: 1,
        last_measurement_unix_ms: 1_712_345_590_000,
        last_success_unix_ms: 1_712_345_300_000,
        last_used_unix_ms: null,
        last_failure_unix_ms: 1_712_345_590_000,
        circuit_open_until_unix_ms: 1_712_345_620_000,
        background_probe_in_flight: false,
        last_background_probe_unix_ms: 1_712_345_590_000,
        last_error: "health probe returned 503"
      },
      {
        index: 2,
        path_kind: "relay_tunnel",
        locator: "relay://node-alpha@https://rendezvous-a.local:9443",
        bootstrap_rank: 0,
        target_node_id: "node-alpha",
        score: 43.6,
        ewma_latency_ms: 18.6,
        ewma_throughput_bytes_per_sec: 160000,
        consecutive_failures: 0,
        total_failures: 0,
        total_successes: 7,
        last_measurement_unix_ms: 1_712_345_598_000,
        last_success_unix_ms: 1_712_345_598_000,
        last_used_unix_ms: 1_712_345_598_500,
        last_failure_unix_ms: null,
        circuit_open_until_unix_ms: null,
        background_probe_in_flight: false,
        last_background_probe_unix_ms: 1_712_345_598_000,
        last_error: null
      },
      {
        index: 3,
        path_kind: "relay_tunnel",
        locator: "relay://node-alpha@https://rendezvous-b.local:9443",
        bootstrap_rank: 1,
        target_node_id: "node-alpha",
        score: 58.4,
        ewma_latency_ms: 33.4,
        ewma_throughput_bytes_per_sec: 120000,
        consecutive_failures: 0,
        total_failures: 1,
        total_successes: 4,
        last_measurement_unix_ms: 1_712_345_592_000,
        last_success_unix_ms: 1_712_345_550_000,
        last_used_unix_ms: null,
        last_failure_unix_ms: 1_712_345_592_000,
        circuit_open_until_unix_ms: null,
        background_probe_in_flight: false,
        last_background_probe_unix_ms: 1_712_345_592_000,
        last_error: null
      },
      {
        index: 4,
        path_kind: "direct_quic",
        locator: "iroh://node-alpha",
        bootstrap_rank: 2,
        target_node_id: "node-alpha",
        score: 74.2,
        ewma_latency_ms: 24.2,
        ewma_throughput_bytes_per_sec: 210000,
        consecutive_failures: 0,
        total_failures: 0,
        total_successes: 2,
        last_measurement_unix_ms: 1_712_345_597_000,
        last_success_unix_ms: 1_712_345_597_000,
        last_used_unix_ms: null,
        last_failure_unix_ms: null,
        circuit_open_until_unix_ms: null,
        background_probe_in_flight: false,
        last_background_probe_unix_ms: 1_712_345_596_000,
        hole_punching_mode: "direct",
        last_error: null
      }
    ]
  };

  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const { pathname, searchParams } = url;
    const method = route.request().method();
    requestedPaths.add(pathname);
    const diagnosticContext = route.request().headers()["x-ironmesh-diagnostic-context"];
    if (diagnosticContext) {
      diagnosticContexts.add(diagnosticContext);
      diagnosticContextRequestCount += 1;
    }

    if (pathname === apiV1("/ping") && method === "GET") {
      return json(route, {
        ok: true,
        service: "cli-client-web",
        backend_version: "0.1.0",
        backend_revision: "v0.1.0-3-gmocked"
      });
    }

    if (pathname === apiV1("/cache-context") && method === "GET") {
      return json(route, {
        schema_version: 1,
        scope: cacheScope
      });
    }

    if (pathname === apiV1("/device-identity") && method === "GET") {
      return json(route, {
        available: true,
        cluster_id: "cluster-dashboard-001",
        device_id: "device-dashboard-001",
        label: "dashboard-device",
        public_key_fingerprint: "public-key-dashboard-fingerprint",
        credential_fingerprint: "credential-dashboard-fingerprint",
        issued_at_unix: 1_700_000_000,
        expires_at_unix: 1_800_000_000,
        rendezvous_mtls_identity_available: true
      });
    }

    if (pathname === apiV1("/health") && method === "GET") {
      return json(route, { mode: "cluster", status: "ok" });
    }

    if (pathname === apiV1("/cluster/status") && method === "GET") {
      return json(route, {
        local_node_id: "node-alpha",
        total_nodes: 2,
        online_nodes: 2,
        offline_nodes: 0,
        policy: {
          replication_factor: 2
        }
      });
    }

    if (
      (pathname === apiV1("/connection-routes") && method === "GET") ||
      (pathname === apiV1("/connection-routes/refresh") && method === "POST")
    ) {
      return json(route, connectionRoutesPayload);
    }

    if (pathname === apiV1("/web-services/nodes") && method === "GET") {
      return json(route, {
        nodeIds: [HOME_NAS_NODE_ID, EMPTY_WEB_SERVICE_NODE_ID]
      });
    }

    if (
      pathname === apiV1(`/web-services/nodes/${HOME_NAS_NODE_ID}`) &&
      method === "GET"
    ) {
      return json(route, {
        nodeId: HOME_NAS_NODE_ID,
        available: true,
        services: [
          {
            id: "home-nas",
            name: "Home NAS",
            description: "Private storage administration",
            nodeId: HOME_NAS_NODE_ID
          }
        ]
      });
    }

    if (
      pathname === apiV1(`/web-services/nodes/${EMPTY_WEB_SERVICE_NODE_ID}`) &&
      method === "GET"
    ) {
      return json(route, {
        nodeId: EMPTY_WEB_SERVICE_NODE_ID,
        available: true,
        services: []
      });
    }

    if (pathname === apiV1("/web-services") && method === "GET") {
      return json(route, [
        {
          id: "home-nas",
          name: "Home NAS",
          description: "Private storage administration",
          nodeId: HOME_NAS_NODE_ID
        }
      ]);
    }

    if (
      pathname ===
        apiV1(`/web-services/${HOME_NAS_NODE_ID}/home-nas/launch`) &&
      method === "POST"
    ) {
      return json(route, {
        url: "about:blank#home-nas",
        expiresInSeconds: 60
      });
    }

    if (pathname === apiV1("/logs") && method === "GET") {
      return json(route, {
        entries: [
          {
            captured_at_unix: 1_700_000_000,
            line: "INFO client_sdk client transport ready"
          },
          {
            captured_at_unix: 1_700_000_002,
            line: [
              "ERROR web_ui_backend health request failed: failed connecting to https://node-alpha.local/api/v1/health",
              "caused by: connection refused"
            ].join("\n")
          }
        ]
      });
    }

    if (pathname === apiV1("/diagnostics/log-export") && method === "GET") {
      return json(route, {
        generated_at_unix: 1_700_000_003,
        requested_window_secs: Number(searchParams.get("window_secs") ?? 180),
        entries: [
          {
            captured_at_unix: 1_700_000_000,
            line: "INFO client_sdk client transport ready"
          },
          {
            captured_at_unix: 1_700_000_002,
            line: "INFO web_ui_backend upstream JSON request finished path=/health outcome=success duration_ms=18"
          }
        ]
      });
    }

    if (pathname === apiV1("/rendezvous/refresh") && method === "POST") {
      return json(route, {
        available: true,
        editable: true,
        transport_mode: "direct",
        relay_mode: "preferred",
        configured_urls: [
          "https://rendezvous-a.local:9443",
          "https://rendezvous-b.local:9443"
        ],
        direct_url: "https://node-alpha.local",
        direct_target_node_id: "node-alpha",
        active_url: null,
        active_target_node_id: null,
        mtls_required: true,
        persistence_source: "bootstrap_file",
        last_probe_error: null,
        endpoint_statuses: [
          {
            url: "https://rendezvous-a.local:9443",
            status: "connected",
            last_attempt_unix: 1_712_345_600,
            last_success_unix: 1_712_345_600,
            consecutive_failures: 0,
            last_error: null,
            active: false
          },
          {
            url: "https://rendezvous-b.local:9443",
            status: "disconnected",
            last_attempt_unix: 1_712_345_598,
            last_success_unix: 1_712_345_420,
            consecutive_failures: 2,
            last_error: "relay probe timed out",
            active: false
          }
        ]
      });
    }

    if (pathname === apiV1("/rendezvous") && method === "GET") {
      return json(route, {
        available: true,
        editable: true,
        transport_mode: "direct",
        relay_mode: "preferred",
        configured_urls: [
          "https://rendezvous-a.local:9443",
          "https://rendezvous-b.local:9443"
        ],
        direct_url: "https://node-alpha.local",
        direct_target_node_id: "node-alpha",
        active_url: null,
        active_target_node_id: null,
        mtls_required: true,
        persistence_source: "bootstrap_file",
        last_probe_error: null,
        endpoint_statuses: [
          {
            url: "https://rendezvous-a.local:9443",
            status: "connected",
            last_attempt_unix: 1_712_345_600,
            last_success_unix: 1_712_345_600,
            consecutive_failures: 0,
            last_error: null,
            active: false
          },
          {
            url: "https://rendezvous-b.local:9443",
            status: "disconnected",
            last_attempt_unix: 1_712_345_598,
            last_success_unix: 1_712_345_420,
            consecutive_failures: 2,
            last_error: "relay probe timed out",
            active: false
          }
        ]
      });
    }

    if (pathname === apiV1("/store/put") && method === "POST") {
      const body = route.request().postDataJSON() as { key: string; value: string };
      if (body.key.endsWith("/")) {
        upsertMockFolderEntry(storeEntries, body.key);
      }
      return json(route, {
        key: body.key,
        size_bytes: body.value.length
      });
    }

    if (pathname === apiV1("/store/get") && method === "GET") {
      if (searchParams.get("preview_bytes")) {
        expect(searchParams.get("preview_bytes")).toBe("1024");
        return json(route, {
          key: searchParams.get("key"),
          value: "hello from the mocked store",
          version: searchParams.get("version"),
          snapshot: searchParams.get("snapshot"),
          truncated: false,
          total_size_bytes: 27,
          preview_size_bytes: 27
        });
      }
      return json(route, {
        key: searchParams.get("key"),
        value: "hello from the mocked store",
        version: searchParams.get("version"),
        snapshot: searchParams.get("snapshot")
      });
    }

    if (pathname === apiV1("/store/delete") && method === "DELETE") {
      deleteMockStorePath(storeEntries, searchParams.get("key") ?? "");
      return json(route, {
        key: searchParams.get("key"),
        deleted: true
      });
    }

    if (pathname === apiV1("/store/rename") && method === "POST") {
      const body = route.request().postDataJSON() as {
        from_path: string;
        to_path: string;
      };
      renameMockStorePath(storeEntries, body.from_path, body.to_path);
      return json(route, {
        from_path: body.from_path,
        to_path: body.to_path,
        renamed: true
      });
    }

    if (pathname === apiV1("/store/history") && method === "GET") {
      return json(route, {
        prefix: searchParams.get("prefix") ?? "",
        depth: Number(searchParams.get("depth") ?? "1"),
        entry_count: historyEntries.length,
        truncated: false,
        entries: historyEntries
      });
    }

    if (pathname === apiV1("/store/history/restore") && method === "POST") {
      historyRestoreRequestCount += 1;
      if (historyRestoreRequestCount === options?.historyRestoreFailureAtCall) {
        return route.fulfill({
          status: 502,
          contentType: "application/json",
          body: JSON.stringify({ error: "simulated history restore failure" })
        });
      }
      const body = route.request().postDataJSON() as {
        entries: Array<{
          path: string;
          restore_source_path: string;
          restore_version_id: string;
        }>;
      };
      const restoredPaths = new Set(body.entries.map((entry) => entry.path));
      const restored = historyEntries.filter((entry) => restoredPaths.has(entry.path));
      restoredHistoryEntries.push(restored);
      historyEntries = historyEntries.filter((entry) => !restoredPaths.has(entry.path));
      return json(route, {
        restored_count: restored.length,
        failed_count: body.entries.length - restored.length,
        entries: body.entries.map((entry) => ({
          ...entry,
          status: restoredPaths.has(entry.path) ? "restored" : "failed"
        }))
      });
    }

    if (pathname === apiV1("/maps/logical-file")) {
      const rangeHeader = route.request().headers().range;
      const commonHeaders = {
        "accept-ranges": "bytes",
        "content-type": "application/octet-stream",
        etag: "\"client-ui-smoke-mbtiles\""
      };

      if (method === "HEAD") {
        await route.fulfill({
          status: 200,
          headers: {
            ...commonHeaders,
            "content-length": String(logicalMapBody.length)
          }
        });
        return;
      }

      if (method === "GET") {
        if (!rangeHeader) {
          await route.fulfill({
            status: 200,
            headers: {
              ...commonHeaders,
              "content-length": String(logicalMapBody.length)
            },
            body: logicalMapBody
          });
          return;
        }

        const match = /^bytes=(\d+)-(\d+)?$/i.exec(rangeHeader);
        expect(match).not.toBeNull();
        const start = Number(match?.[1] ?? "0");
        const inclusiveEnd = Math.min(
          Number(match?.[2] ?? String(logicalMapBody.length - 1)),
          logicalMapBody.length - 1
        );
        const sliced = logicalMapBody.subarray(start, inclusiveEnd + 1);
        await route.fulfill({
          status: 206,
          headers: {
            ...commonHeaders,
            "content-length": String(sliced.length),
            "content-range": `bytes ${start}-${inclusiveEnd}/${logicalMapBody.length}`
          },
          body: sliced
        });
        return;
      }
    }

    if (pathname === apiV1("/maps/config") && method === "GET") {
      if (options?.mapConfigurationStatus) {
        await route.fulfill({
          status: options.mapConfigurationStatus,
          contentType: "application/json",
          body: JSON.stringify({ message: "mocked map configuration failure" })
        });
        return;
      }

      return json(route, {
        stored: true,
        configuration: options?.mapConfiguration ?? defaultGalleryMapConfiguration()
      });
    }

    if (pathname === apiV1("/maps/mbtiles-metadata") && method === "GET") {
      if (options?.mapMetadataStatus) {
        await route.fulfill({
          status: options.mapMetadataStatus,
          contentType: "application/json",
          body: JSON.stringify({ message: "mocked map metadata failure" })
        });
        return;
      }

      return json(route, {
        attribution: "Made with Natural Earth.",
        center: options?.mapMetadataCenter ?? [0, 20, 1],
        format: "png",
        minzoom: 0,
        maxzoom: 2
      });
    }

    if (pathname.startsWith(apiV1("/maps/tiles/")) && method === "GET") {
      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "image/png",
          "cache-control": "public, max-age=3600"
        },
        body: imageBody
      });
      return;
    }

    if (pathname.startsWith(apiV1("/maps/vector-tiles/")) && method === "GET") {
      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "application/vnd.mapbox-vector-tile",
          "content-encoding": "gzip",
          "cache-control": "public, max-age=3600"
        },
        body: emptyVectorTileBody
      });
      return;
    }

    if (pathname.startsWith(apiV1("/maps/fonts/")) && method === "GET") {
      await route.fulfill({
        status: 200,
        headers: {
          "content-type": "application/x-protobuf",
          "cache-control": "public, max-age=3600"
        },
        body: glyphRangeBody
      });
      return;
    }

    if (
      pathname === apiV1("/gallery/map/clusters") &&
      method === "GET" &&
      options?.legacyGalleryMapApiOnly
    ) {
      await route.fulfill({ status: 404, contentType: "application/json", body: "{}" });
      return;
    }

    if (
      method === "GET" &&
      ((pathname === apiV1("/gallery/map/clusters") && !options?.legacyGalleryMapApiOnly) ||
        (pathname === apiV1("/store/map/clusters") && options?.legacyGalleryMapApiOnly))
    ) {
      galleryMapClusterRequestCount += 1;
      if (galleryMapClusterRequestCount > 1 && options?.mapClusterRefreshDelayMs) {
        await new Promise((resolve) => setTimeout(resolve, options.mapClusterRefreshDelayMs));
      }
      return json(route, galleryMapMock.clusters(storeEntries, searchParams));
    }

    if (
      pathname === apiV1("/gallery/map/cluster-entries") &&
      method === "GET" &&
      options?.legacyGalleryMapApiOnly
    ) {
      await route.fulfill({ status: 404, contentType: "application/json", body: "{}" });
      return;
    }

    if (
      method === "GET" &&
      ((pathname === apiV1("/gallery/map/cluster-entries") && !options?.legacyGalleryMapApiOnly) ||
        (pathname === apiV1("/store/map/cluster-entries") && options?.legacyGalleryMapApiOnly))
    ) {
      if (options?.mapClusterEntriesDelayMs) {
        await new Promise((resolve) => setTimeout(resolve, options.mapClusterEntriesDelayMs));
      }
      return json(
        route,
        galleryMapMock.clusterEntries(searchParams)
      );
    }

    if (pathname === apiV1("/store/list") && method === "GET") {
      expect(searchParams.get("view")).toBe("tree");
      galleryStoreListRequestCount += 1;
      if (galleryOffline) {
        await route.fulfill({
          status: 503,
          contentType: "application/json",
          body: JSON.stringify({ message: "mock gallery is offline" })
        });
        return;
      }
      const storeListDelay =
        galleryStoreListDelayByMediaFilter.get(searchParams.get("media_filter") ?? "") ??
        galleryStoreListDelayMs;
      if (storeListDelay > 0) {
        await new Promise((resolve) => setTimeout(resolve, storeListDelay));
      }
      const response = buildMockStoreListResponse(storeEntries, searchParams);
      return json(route, response);
    }

    if ((pathname === apiV1("/media/thumbnail") || pathname === "/media/thumbnail") && method === "GET") {
      await route.fulfill({
        status: 200,
        contentType: "image/png",
        body: imageBody
      });
      return;
    }

    if (pathname === apiV1("/snapshots") && method === "GET") {
      if (galleryOffline) {
        await route.fulfill({
          status: 503,
          contentType: "application/json",
          body: JSON.stringify({ message: "mock gallery is offline" })
        });
        return;
      }
      return json(route, [{ id: "snapshot-001" }]);
    }

    if (pathname === apiV1("/versions") && method === "GET") {
      return json(
        route,
        buildMockVersionGraphResponse(
          searchParams.get("key") ?? "",
          currentVersionByKey.get(searchParams.get("key") ?? "") ?? null
        )
      );
    }

    if (pathname.startsWith(`${apiV1("/versions")}/`) && pathname.includes("/restore/") && method === "POST") {
      const versionPrefix = `${apiV1("/versions")}/`;
      const [encodedKey, encodedVersionId] = pathname.slice(versionPrefix.length).split("/restore/");
      const key = decodeURIComponent(encodedKey ?? "");
      const versionId = decodeURIComponent(encodedVersionId ?? "");
      const body = route.request().postDataJSON() as {
        to_path: string;
        overwrite?: boolean;
      };
      restoredVersions.push({ key, versionId, targetPath: body.to_path });
      await route.fulfill({ status: 204, body: "" });
      return;
    }

    if (pathname === apiV1("/cluster/nodes") && method === "GET") {
      return json(route, [
        { node_id: "node-alpha", status: "online" },
        { node_id: "node-beta", status: "online" }
      ]);
    }

    if (pathname === apiV1("/cluster/replication/plan") && method === "GET") {
      return json(route, {
        under_replicated: 1,
        over_replicated: 0,
        items: [{ key: "docs/readme.txt" }]
      });
    }

    if (pathname === apiV1("/store/uploads/start") && method === "POST") {
      uploadSessionStartCount += 1;
      const body = route.request().postDataJSON() as {
        key: string;
        total_size_bytes: number;
      };
      const uploadId = `upload-${uploadSessionStartCount}`;
      uploadSizes.set(uploadId, body.total_size_bytes);
      uploadKeys.set(uploadId, body.key);
      return json(route, {
        upload_id: uploadId,
        key: body.key,
        total_size_bytes: body.total_size_bytes,
        chunk_size_bytes: 4,
        chunk_count: Math.ceil(body.total_size_bytes / 4),
        received_indexes: [],
        completed: false
      });
    }

    if (/^\/api\/v1\/store\/uploads\/[^/]+\/chunk\/\d+$/.test(pathname) && method === "PUT") {
      const uploadId = pathname.split("/")[5] ?? "upload-unknown";
      const index = Number(pathname.split("/").pop() ?? "0");
      activeUploadIds.add(uploadId);
      maxConcurrentUploadIds = Math.max(maxConcurrentUploadIds, activeUploadIds.size);
      try {
        await new Promise((resolve) => setTimeout(resolve, 75));
        await json(route, {
          stored: true,
          received_index: index
        });
      } catch {
        return;
      } finally {
        activeUploadIds.delete(uploadId);
      }
      return;
    }

    if (/^\/api\/v1\/store\/uploads\/[^/]+\/complete$/.test(pathname) && method === "POST") {
      const uploadId = pathname.split("/")[5] ?? "upload-unknown";
      const key = uploadKeys.get(uploadId) ?? `uploads/${uploadId}.bin`;
      const totalSizeBytes = uploadSizes.get(uploadId) ?? 21;
      upsertMockBinaryEntry(storeEntries, key, totalSizeBytes);
      return json(route, {
        snapshot_id: "snapshot-001",
        version_id: "version-002",
        manifest_hash: "manifest-upload",
        state: "ready",
        new_chunks: Math.ceil(totalSizeBytes / 4),
        dedup_reused_chunks: 0,
        created_new_version: true,
        total_size_bytes: totalSizeBytes
      });
    }

    if (/^\/api\/v1\/store\/uploads\/[^/]+$/.test(pathname) && method === "DELETE") {
      deletedUploadSessionIds.add(pathname.split("/")[5] ?? "upload-unknown");
      await route.fulfill({
        status: 204
      });
      return;
    }

    if (pathname === apiV1("/store/stream-binary") && method === "GET") {
      if (
        searchParams.get("key") === "gallery/cat.png" ||
        searchParams.get("key") === "gallery/dog.jpg"
      ) {
        await new Promise((resolve) => setTimeout(resolve, 250));
        await route.fulfill({
          status: 200,
          contentType: "image/png",
          body: imageBody
        });
        return;
      }
      if (searchParams.get("key") === "gallery/ios-photo.heic") {
        await route.fulfill({
          status: 200,
          contentType: "image/heic",
          body: Buffer.from("mock-heic-payload")
        });
        return;
      }
      if (searchParams.get("key") === "gallery/clip.mp4") {
        await route.fulfill({
          status: 200,
          contentType: "video/mp4",
          headers: {
            "accept-ranges": "bytes",
            "content-disposition": 'inline; filename="clip.mp4"'
          },
          body: movieBody
        });
        return;
      }
    }

    if (pathname === apiV1("/store/get-binary") && method === "GET") {
      await route.fulfill({
        status: 200,
        contentType: "application/octet-stream",
        headers: {
          "content-disposition": "attachment; filename=\"mock.bin\""
        },
        body: "mock-binary"
      });
      return;
    }

    return route.continue();
  });

  return {
    maxConcurrentUploadIds: () => maxConcurrentUploadIds,
    deletedUploadSessionIds: () => Array.from(deletedUploadSessionIds),
    requestedPaths: () => Array.from(requestedPaths),
    diagnosticContexts: () => Array.from(diagnosticContexts),
    diagnosticContextRequestCount: () => diagnosticContextRequestCount,
    restoredVersions: () => restoredVersions.slice(),
    restoredHistoryEntries: () => restoredHistoryEntries.slice(),
    galleryStoreListRequestCount: () => galleryStoreListRequestCount,
    replaceStoreEntries: (entries: MockStoreEntry[]) => {
      storeEntries.splice(0, storeEntries.length, ...entries);
    },
    setGalleryOffline: (offline: boolean) => {
      galleryOffline = offline;
    },
    setGalleryStoreListDelay: (delayMs: number) => {
      galleryStoreListDelayMs = delayMs;
    },
    setGalleryStoreListDelayForMediaFilter: (mediaFilter: string, delayMs: number) => {
      galleryStoreListDelayByMediaFilter.set(mediaFilter, delayMs);
    },
    setCacheScope: (scope: string | null) => {
      cacheScope = scope;
    }
  };
}

async function maxGalleryVirtualPageGap(page: Page): Promise<number> {
  return page.locator('[data-gallery-virtual-page-slot="true"]').evaluateAll((nodes) => {
    const gaps = nodes
      .map((node) => {
        const slot = node as HTMLElement;
        const pageGrid = slot.querySelector('[data-gallery-grid="true"]');
        if (!(pageGrid instanceof HTMLElement)) {
          return null;
        }

        return Math.max(
          0,
          slot.getBoundingClientRect().height - pageGrid.getBoundingClientRect().height
        );
      })
      .filter((gap): gap is number => gap !== null);

    return gaps.length > 0 ? Math.max(...gaps) : Number.POSITIVE_INFINITY;
  });
}

async function json(route: Route, payload: unknown) {
  await route.fulfill({
    status: 200,
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(payload)
  });
}

type MockStoreEntry = {
  path: string;
  entry_type: "prefix" | "key";
  version?: string;
  size_bytes?: number;
  modified_at_unix?: number;
  media?: Record<string, unknown>;
};

function createMockStoreEntries(): MockStoreEntry[] {
  return [
    { path: "docs/", entry_type: "prefix" },
    {
      path: "docs/readme.txt",
      entry_type: "key",
      version: "version-readme-001",
      size_bytes: 23,
      modified_at_unix: 1_712_345_600
    },
    { path: "docs/nested/", entry_type: "prefix" },
    { path: "media/", entry_type: "prefix" },
    {
      path: "gallery/cat.png",
      entry_type: "key",
      version: "version-cat-001",
      size_bytes: 3_145_728,
      modified_at_unix: 1_712_345_678,
      media: {
        status: "ready",
        content_fingerprint: "fingerprint-cat",
        media_type: "image",
        mime_type: "image/png",
        width: 1024,
        height: 768,
        taken_at_unix: 1712345678,
        gps: {
          latitude: 47.3769,
          longitude: 8.5417
        },
        thumbnail: {
          url: "/media/thumbnail?key=gallery%2Fcat.png",
          profile: "grid",
          width: 256,
          height: 192,
          format: "jpeg",
          size_bytes: 1234
        }
      }
    },
    {
      path: "gallery/dog.jpg",
      entry_type: "key",
      version: "version-dog-001",
      size_bytes: 2_048,
      modified_at_unix: 1_712_300_000,
      media: {
        status: "pending",
        content_fingerprint: "fingerprint-dog",
        media_type: "image",
        mime_type: "image/jpeg",
        gps: {
          latitude: 40.7128,
          longitude: -74.006
        },
        thumbnail: {
          url: "/media/thumbnail?key=gallery%2Fdog.jpg",
          profile: "grid",
          width: 256,
          height: 256,
          format: "jpeg",
          size_bytes: 0
        }
      }
    },
    {
      path: "gallery/clip.mp4",
      entry_type: "key",
      version: "version-clip-001",
      size_bytes: 48_000_000,
      modified_at_unix: 1_712_250_000,
      media: {
        status: "ready",
        content_fingerprint: "fingerprint-clip",
        media_type: "video",
        mime_type: "video/mp4",
        width: 1920,
        height: 1080
      }
    }
  ];
}

function createRevalidatedGalleryMockStoreEntries(): MockStoreEntry[] {
  return createMockStoreEntries().map((entry) => {
    if (entry.path !== "gallery/cat.png" || !entry.media) {
      return entry;
    }
    return {
      ...entry,
      path: "gallery/revalidated.png",
      media: {
        ...entry.media,
        content_fingerprint: "fingerprint-revalidated",
        thumbnail: {
          url: "/media/thumbnail?key=gallery%2Frevalidated.png",
          profile: "grid",
          width: 256,
          height: 192,
          format: "jpeg",
          size_bytes: 1234
        }
      }
    };
  });
}

async function expireGalleryCacheSchema(page: Page): Promise<void> {
  await page.evaluate(
    () =>
      new Promise<void>((resolve, reject) => {
        const request = indexedDB.open("ironmesh-client-gallery-cache", 1);
        request.onerror = () => reject(request.error);
        request.onsuccess = () => {
          const database = request.result;
          const transaction = database.transaction("records", "readwrite");
          const store = transaction.objectStore("records");
          const recordsRequest = store.getAll();
          recordsRequest.onerror = () => reject(recordsRequest.error);
          recordsRequest.onsuccess = () => {
            for (const record of recordsRequest.result as Array<Record<string, unknown>>) {
              store.put({ ...record, schemaVersion: 0, payload: { corrupt: true } });
            }
          };
          transaction.oncomplete = () => {
            database.close();
            resolve();
          };
          transaction.onerror = () => reject(transaction.error);
          transaction.onabort = () => reject(transaction.error);
        };
      })
  );
}

async function galleryCacheDatabaseExists(page: Page): Promise<boolean> {
  return page.evaluate(async () => {
    if (!("databases" in indexedDB)) {
      return false;
    }
    const databases = await indexedDB.databases();
    return databases.some((database) => database.name === "ironmesh-client-gallery-cache");
  });
}

function createGalleryPaginationMockStoreEntries(mediaCount: number): MockStoreEntry[] {
  return [
    ...createMockStoreEntries(),
    ...Array.from({ length: mediaCount }, (_, index) => {
      const path = `gallery/paginated-${String(index + 1).padStart(3, "0")}.jpg`;
      return {
        path,
        entry_type: "key" as const,
        size_bytes: 256_000 + index,
        modified_at_unix: 1_712_200_000 - index,
        media: {
          status: "ready",
          content_fingerprint: `fingerprint-paginated-${index + 1}`,
          media_type: "image",
          mime_type: "image/jpeg",
          width: 1600,
          height: 1200,
          taken_at_unix: 1_712_100_000 - index,
          thumbnail: {
            url: `/media/thumbnail?key=${encodeURIComponent(path)}`,
            profile: "grid",
            width: 256,
            height: 192,
            format: "jpeg",
            size_bytes: 1234
          }
        }
      };
    })
  ];
}

function createHeicGalleryMockStoreEntries(): MockStoreEntry[] {
  return createMockStoreEntries().map((entry) => {
    if (entry.path !== "gallery/cat.png" || !entry.media) {
      return entry;
    }

    return {
      ...entry,
      path: "gallery/ios-photo.heic",
      size_bytes: 2_621_440,
      modified_at_unix: 1_712_360_000,
      media: {
        ...entry.media,
        content_fingerprint: "fingerprint-ios-photo",
        mime_type: "image/heic",
        width: 4032,
        height: 3024,
        taken_at_unix: 1_712_359_900,
        gps: {
          latitude: 37.7858,
          longitude: -122.4064
        },
        thumbnail: {
          url: "/media/thumbnail?key=gallery%2Fios-photo.heic",
          profile: "grid",
          width: 256,
          height: 192,
          format: "jpeg",
          size_bytes: 1234
        }
      }
    };
  });
}

function buildMockStoreListResponse(entries: MockStoreEntry[], searchParams: URLSearchParams) {
  const prefix = searchParams.get("prefix") ?? "";
  const depth = Number(searchParams.get("depth") ?? "1");
  const mediaFilter = searchParams.get("media_filter");
  const isTreeNavigationRequest =
    searchParams.get("view") === "tree" &&
    !searchParams.has("offset") &&
    !searchParams.has("limit") &&
    !searchParams.has("sort") &&
    !mediaFilter;
  const scopedEntries = isTreeNavigationRequest
    ? projectMockStoreTreeEntries(entries, prefix, depth)
    : filterMockStoreEntriesToPrefix(entries, prefix);
  const filteredEntries = mediaFilter
    ? scopedEntries.filter((entry) => matchesMockMediaFilter(entry, mediaFilter))
    : scopedEntries;
  const sortedEntries = sortMockGalleryEntries(filteredEntries, searchParams.get("sort"));
  const totalEntryCount = sortedEntries.length;
  const offset = Math.max(0, Number(searchParams.get("offset") ?? "0") || 0);
  const limitParam = searchParams.get("limit");
  const limit = limitParam ? Math.max(1, Number(limitParam) || 1) : null;
  const pagedEntries =
    typeof limit === "number"
      ? sortedEntries.slice(offset, offset + limit)
      : sortedEntries.slice(offset);

  return {
    prefix,
    depth,
    entry_count: pagedEntries.length,
    total_entry_count: totalEntryCount,
    offset,
    limit,
    has_more: offset + pagedEntries.length < totalEntryCount,
    consistency_token: "mock-store-revision-1",
    media_summary: summarizeMockGalleryEntries(filteredEntries),
    entries: pagedEntries
  };
}

function buildMockVersionGraphResponse(key: string, preferredHeadVersionId: string | null) {
  if (key === "gallery/cat.png") {
    return {
      key,
      preferred_head_version_id: preferredHeadVersionId,
      versions: [
        {
          version_id: "version-cat-001",
          entry_type: "key",
          size_bytes: 3_145_728,
          modified_at_unix: 1_712_345_678,
          created_at_unix: 1_712_345_678,
          media: {
            status: "ready",
            media_type: "image",
            mime_type: "image/png",
            thumbnail: {
              url: "/media/thumbnail?key=gallery%2Fcat.png&version=version-cat-001",
              profile: "grid",
              width: 256,
              height: 192,
              format: "jpeg",
              size_bytes: 1234
            }
          }
        },
        {
          version_id: "version-cat-000",
          entry_type: "key",
          size_bytes: 3_145_728,
          modified_at_unix: 1_712_300_000,
          created_at_unix: 1_712_300_000,
          media: {
            status: "ready",
            media_type: "image",
            mime_type: "image/png",
            thumbnail: {
              url: "/media/thumbnail?key=gallery%2Fcat.png&version=version-cat-000",
              profile: "grid",
              width: 256,
              height: 192,
              format: "jpeg",
              size_bytes: 1234
            }
          }
        }
      ]
    };
  }

  return {
    key,
    preferred_head_version_id: preferredHeadVersionId,
    versions: [
      {
        version_id: "version-001",
        entry_type: "key",
        size_bytes: 23,
        modified_at_unix: 1_712_345_600,
        created_at_unix: 1_712_345_600
      },
      {
        version_id: "version-000",
        entry_type: "key",
        size_bytes: 21,
        modified_at_unix: 1_712_300_000,
        created_at_unix: 1_712_300_000
      }
    ]
  };
}

function matchesMockMediaFilter(entry: MockStoreEntry, mediaFilter: string): boolean {
  const media = entry.media;
  if (!media) {
    return false;
  }

  if (mediaFilter === "all") {
    return true;
  }

  return media.media_type === mediaFilter;
}

function sortMockGalleryEntries(entries: MockStoreEntry[], sort: string | null): MockStoreEntry[] {
  const sorted = [...entries];

  if (sort === "path_asc") {
    sorted.sort((left, right) => left.path.localeCompare(right.path));
    return sorted;
  }

  if (sort === "path_desc") {
    sorted.sort((left, right) => right.path.localeCompare(left.path));
    return sorted;
  }

  if (sort === "captured_asc") {
    sorted.sort(
      (left, right) =>
        (left.modified_at_unix ?? 0) - (right.modified_at_unix ?? 0) ||
        left.path.localeCompare(right.path)
    );
    return sorted;
  }

  sorted.sort(
    (left, right) =>
      (right.modified_at_unix ?? 0) - (left.modified_at_unix ?? 0) ||
      left.path.localeCompare(right.path)
  );
  return sorted;
}

function summarizeMockGalleryEntries(entries: MockStoreEntry[]) {
  return entries.reduce(
    (summary, entry) => {
      const media = entry.media;
      if (!media) {
        return summary;
      }

      if (media.status === "ready") {
        summary.ready_count += 1;
      } else if (media.status === "pending") {
        summary.pending_count += 1;
      } else {
        summary.incomplete_count += 1;
      }

      if (media.media_type === "image") {
        summary.image_count += 1;
      }
      if (media.media_type === "video") {
        summary.video_count += 1;
      }
      if (media.gps) {
        summary.geotagged_count += 1;
      }

      return summary;
    },
    {
      ready_count: 0,
      pending_count: 0,
      incomplete_count: 0,
      image_count: 0,
      video_count: 0,
      geotagged_count: 0
    }
  );
}

function upsertMockFolderEntry(entries: MockStoreEntry[], key: string) {
  const normalized = normalizeMockFolderKey(key);
  if (!normalized) {
    return;
  }
  const existing = entries.find((entry) => entry.path === normalized);
  if (existing) {
    existing.entry_type = "prefix";
    return;
  }
  entries.push({
    path: normalized,
    entry_type: "prefix"
  });
}

function upsertMockBinaryEntry(entries: MockStoreEntry[], key: string, sizeBytes: number) {
  const normalized = key.trim();
  if (!normalized) {
    return;
  }
  const existing = entries.find((entry) => entry.path === normalized);
  if (existing) {
    existing.entry_type = "key";
    existing.size_bytes = sizeBytes;
    existing.modified_at_unix = 1_712_345_900;
    delete existing.media;
    return;
  }
  entries.push({
    path: normalized,
    entry_type: "key",
    size_bytes: sizeBytes,
    modified_at_unix: 1_712_345_900
  });
}

function deleteMockStorePath(entries: MockStoreEntry[], key: string) {
  const normalized = key.trim();
  if (!normalized) {
    return;
  }
  if (normalized.endsWith("/")) {
    const survivors = entries.filter((entry) => !entry.path.startsWith(normalized));
    entries.splice(0, entries.length, ...survivors);
    return;
  }
  const survivors = entries.filter((entry) => entry.path !== normalized);
  entries.splice(0, entries.length, ...survivors);
}

function renameMockStorePath(entries: MockStoreEntry[], fromPath: string, toPath: string) {
  const normalizedFrom = fromPath.trim();
  const normalizedTo = toPath.trim();
  if (!normalizedFrom || !normalizedTo) {
    return;
  }

  const entry = entries.find((candidate) => candidate.path === normalizedFrom);
  if (entry) {
    entry.path = normalizedTo;
  }
}

function normalizeMockFolderKey(key: string): string {
  const normalized = key
    .split("/")
    .map((segment) => segment.trim())
    .filter(Boolean)
    .join("/");
  return normalized ? `${normalized}/` : "";
}

function tinyPngBuffer(): Buffer {
  return Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO7Z0N8AAAAASUVORK5CYII=",
    "base64"
  );
}
