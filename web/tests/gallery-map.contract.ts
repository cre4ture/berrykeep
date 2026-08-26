import type { GalleryMapConfiguration } from "@ironmesh/api";
import { expect, test, type Page } from "@playwright/test";

export type GalleryMapContractSetup = {
  mapConfiguration?: GalleryMapConfiguration;
  mapConfigurationStatus?: number;
};

export type GalleryMapContractTarget = {
  name: string;
  setup: (page: Page, setup: GalleryMapContractSetup) => Promise<void>;
  setupInitialOverviewScenario: (page: Page) => Promise<void>;
  openGallery: (page: Page) => Promise<void>;
};

const configuredMapVariants: GalleryMapConfiguration = {
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
      raster_manifest_key: "sys/maps/natural-earth-globe.mbtiles.manifest.json"
    },
    {
      id: "natural-earth-labels",
      label: "Natural Earth Globe + labels",
      mode_label: "Labels",
      description: "Natural Earth base map with country, city, and border labels.",
      attribution: "Made with Natural Earth.",
      kind: "hybrid",
      style: "natural_earth",
      enabled: true,
      raster_manifest_key: "sys/maps/natural-earth-globe.mbtiles.manifest.json",
      vector_manifest_key: "sys/maps/natural-earth-labels.mbtiles.manifest.json"
    },
    {
      id: "natural-earth-vector",
      label: "Natural Earth Vector",
      mode_label: "Vector",
      description: "Natural Earth physical world map rendered from vector tiles.",
      attribution: "Made with Natural Earth.",
      kind: "vector",
      style: "natural_earth",
      enabled: true,
      vector_manifest_key: "sys/maps/natural-earth-vector.mbtiles.manifest.json"
    },
    {
      id: "natural-earth-1",
      label: "Natural Earth I Relief + Water",
      mode_label: "Relief I",
      description: "Natural Earth I land cover with shaded relief and water.",
      attribution: "Made with Natural Earth.",
      kind: "raster",
      style: "raster",
      enabled: true,
      raster_manifest_key: "sys/maps/natural-earth-one.mbtiles.manifest.json"
    },
    {
      id: "openmaptiles-street",
      label: "OpenMapTiles Street",
      mode_label: "Street",
      description: "Detailed global OpenMapTiles street map.",
      attribution: "Map data © OpenStreetMap contributors.",
      kind: "vector",
      style: "openmaptiles",
      enabled: true,
      vector_manifest_key: "sys/maps/openmaptiles-street.mbtiles.manifest.json"
    },
    {
      id: "hidden-operator-map",
      label: "Hidden operator map",
      mode_label: "Hidden",
      description: "A configured variant that is intentionally not visible to gallery users.",
      attribution: "Internal test data.",
      kind: "raster",
      style: "raster",
      enabled: false,
      raster_manifest_key: "sys/maps/hidden-operator-map.mbtiles.manifest.json"
    }
  ]
};

/**
 * Registers the shared gallery-map behavior that both the client and admin
 * surfaces must provide. Each target owns authentication and HTTP mocking;
 * the assertions intentionally stay identical.
 */
export function registerGalleryMapContractTests(target: GalleryMapContractTarget): void {
  test(`${target.name} gallery map contract lists visible configured styles`, async ({ page }) => {
    await target.setup(page, { mapConfiguration: configuredMapVariants });
    await target.openGallery(page);
    await expect(page.getByLabel("Depth")).toHaveValue("64");
    const requestedPreciseMapZooms: number[] = [];
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (!url.pathname.endsWith("/gallery/map/clusters")) {
        return;
      }
      const zoom = Number(url.searchParams.get("zoom_precise"));
      if (Number.isFinite(zoom)) {
        requestedPreciseMapZooms.push(zoom);
      }
    });
    const firstMapClusterRequest = page.waitForRequest((request) => {
      const url = new URL(request.url());
      return url.pathname.endsWith("/gallery/map/clusters");
    });
    await page.getByRole("button", { name: "Map" }).click();
    const firstMapClusterUrl = new URL((await firstMapClusterRequest).url());
    expect(firstMapClusterUrl.searchParams.get("depth")).toBe("64");
    expect(firstMapClusterUrl.searchParams.get("media_filter")).toBe("all");
    expect(firstMapClusterUrl.searchParams.get("zoom")).toBe("1");
    expect(firstMapClusterUrl.searchParams.get("zoom_precise")).toBe("1");
    expect(firstMapClusterUrl.searchParams.has("offset")).toBe(false);
    expect(firstMapClusterUrl.searchParams.has("limit")).toBe(false);
    const mapCanvas = page.locator(".maplibregl-canvas");
    await expect(mapCanvas).toBeVisible();
    const fractionalMapRequest = page.waitForRequest((request) => {
      const url = new URL(request.url());
      const zoom = Number(url.searchParams.get("zoom_precise"));
      return (
        url.pathname.endsWith("/gallery/map/clusters") &&
        Number.isFinite(zoom) &&
        !Number.isInteger(zoom)
      );
    });
    await mapCanvas.hover();
    await page.mouse.wheel(0, -120);
    await fractionalMapRequest;
    expect(requestedPreciseMapZooms.some((zoom) => !Number.isInteger(zoom))).toBe(true);

    await expect(page.getByText("Map styles could not be refreshed")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Retry map styles" })).toHaveCount(0);

    const mapDisplay = page.getByRole("textbox", { name: "Map display", exact: true });
    await expect(mapDisplay).toHaveValue("Natural Earth Globe");
    await mapDisplay.click();
    await expect(page.getByRole("option", { name: "Natural Earth Globe + labels" })).toBeVisible();
    await expect(page.getByRole("option", { name: "Natural Earth Vector" })).toBeVisible();
    await expect(page.getByRole("option", { name: "Natural Earth I Relief + Water" })).toBeVisible();
    await expect(page.getByRole("option", { name: "OpenMapTiles Street" })).toBeVisible();
    await expect(page.getByRole("option", { name: "Hidden operator map" })).toHaveCount(0);
    await page.getByRole("option", { name: "Natural Earth Globe + labels" }).click();
    await expect(mapDisplay).toHaveValue("Natural Earth Globe + labels");
    await mapDisplay.click();
    await page.getByRole("option", { name: "Natural Earth Vector" }).click();
    await expect(mapDisplay).toHaveValue("Natural Earth Vector");
    await mapDisplay.click();
    await page.getByRole("option", { name: "Natural Earth I Relief + Water" }).click();
    await expect(mapDisplay).toHaveValue("Natural Earth I Relief + Water");

    await page.getByRole("button", { name: "Fullscreen map" }).click();
    await expect(mapDisplay).toBeVisible();
    const mapDisplayControls = page.locator('[data-gallery-map-display-controls="true"]');
    expect(
      await mapDisplayControls.evaluate((element) => element.parentElement?.parentElement?.tagName)
    ).toBe("BODY");
  });

  test(`${target.name} gallery map contract keeps configuration failures explicit`, async ({ page }) => {
    await target.setup(page, { mapConfigurationStatus: 503 });
    await target.openGallery(page);
    await page.getByRole("button", { name: "Map" }).click();

    await expect(page.getByText("Gallery map styles are unavailable")).toBeVisible();
    await expect(
      page.getByText(/The map configuration could not be loaded\. HTTP 503/)
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Retry map styles" })).toBeVisible();
    await expect(page.locator('[aria-label="Geotagged gallery map"]')).toHaveCount(0);
  });

  test(`${target.name} gallery map contract fits the initial server overview and opens cluster choices`, async ({
    page
  }) => {
    await target.setupInitialOverviewScenario(page);
    await target.openGallery(page);
    await page.getByRole("button", { name: "Map" }).click();

    const clusterButtons = page.getByRole("button", { name: "Open map cluster with 2 items" });
    await expect(clusterButtons).toHaveCount(2);
    await expect(clusterButtons.first()).toBeVisible();
    await expect(clusterButtons.nth(1)).toBeVisible();

    await clusterButtons.first().click();
    const chooser = page.getByRole("dialog", { name: "2 items in map cluster" });
    await expect(chooser).toBeVisible();
    await expect(chooser.getByRole("button", { name: "gallery/new-york-a.png" })).toBeVisible();
    await expect(chooser.getByRole("button", { name: "gallery/new-york-b.png" })).toBeVisible();
    await expect(clusterButtons).toHaveCount(2);

    await page.keyboard.press("Escape");
    await expect(chooser).toHaveCount(0);
    await clusterButtons.first().click({ modifiers: ["Control"] });
    await expect(chooser).toHaveCount(0);
    await expect(clusterButtons).toHaveCount(0);
  });

  test(`${target.name} gallery map contract drills in from a Ctrl-click context menu`, async ({
    page
  }) => {
    await target.setupInitialOverviewScenario(page);
    await target.openGallery(page);
    await page.getByRole("button", { name: "Map" }).click();

    const cluster = page.getByRole("button", { name: "Open map cluster with 2 items" }).first();
    await expect(cluster).toBeVisible();
    await cluster.evaluate((element) => {
      element.dispatchEvent(
        new MouseEvent("contextmenu", {
          bubbles: true,
          cancelable: true,
          button: 2,
          ctrlKey: true
        })
      );
    });

    await expect(page.getByRole("dialog", { name: "2 items in map cluster" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Open map cluster with 2 items" })).toHaveCount(0);
  });
}

export function createInitialOverviewGalleryEntries() {
  return [
    createGalleryEntry("gallery/new-york-a.png", 40.7128, -74.006),
    createGalleryEntry("gallery/new-york-b.png", 40.7628, -74.056),
    createGalleryEntry("gallery/tokyo-a.png", 35.6762, 139.6503),
    createGalleryEntry("gallery/tokyo-b.png", 35.7262, 139.7003)
  ];
}

function createGalleryEntry(path: string, latitude: number, longitude: number) {
  return {
    path,
    entry_type: "key" as const,
    modified_at_unix: 1_712_345_678,
    media: {
      status: "ready",
      content_fingerprint: `fingerprint-${path}`,
      media_type: "image",
      mime_type: "image/png",
      width: 1,
      height: 1,
      taken_at_unix: 1_712_345_678,
      gps: { latitude, longitude },
      thumbnail: {
        url: `/api/v1/media/thumbnail?key=${encodeURIComponent(path)}`,
        profile: "grid",
        width: 1,
        height: 1,
        format: "png",
        size_bytes: 68
      }
    }
  };
}
