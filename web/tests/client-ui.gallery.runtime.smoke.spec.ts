import { expect, test } from "@playwright/test";

test("client-ui runtime fits the gallery overview through the client proxy", async ({ page }) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => {
    pageErrors.push(error.message);
  });

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();

  await page.getByText("Gallery", { exact: true }).click();
  await expect(page.getByRole("heading", { name: "Gallery" })).toBeVisible();
  await expect(page.getByText("gallery/runtime-map-a.png", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Map" }).click();
  const mapDisplay = page.getByRole("textbox", { name: "Map display", exact: true });
  await expect(mapDisplay).toHaveValue("Natural Earth Globe");
  await mapDisplay.click();
  await expect(page.getByRole("option", { name: "Natural Earth Globe + labels" })).toBeVisible();
  await expect(page.getByRole("option", { name: "OpenMapTiles Street" })).toBeVisible();
  await expect(page.getByText("Gallery map styles are unavailable")).toHaveCount(0);

  const clusters = page.getByRole("button", { name: "Open map cluster with 2 items" });
  await expect(clusters).toHaveCount(2);
  await clusters.first().click();
  const chooser = page.getByRole("dialog", { name: "2 items in map cluster" });
  await expect(chooser).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-a.png" })).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-b.png" })).toBeVisible();
  expect(pageErrors).toEqual([]);
});

test("client-ui runtime keeps the map photo viewer visible in an embedded viewport", async ({ page }) => {
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

  await page.goto("/?embedded_client=android");
  await page.getByText("Gallery", { exact: true }).click();
  await page.getByRole("button", { name: "Map" }).click();
  await page.getByRole("button", { name: "Fullscreen map" }).click();
  await expect(page.getByRole("button", { name: "Exit fullscreen map" })).toBeVisible();

  const cluster = page.getByRole("button", { name: "Open map cluster with 2 items" }).first();
  await expect(cluster).toBeVisible();
  await cluster.click();

  const chooser = page.getByRole("dialog", { name: "2 items in map cluster" });
  await expect(chooser).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-a.png" })).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-b.png" })).toBeVisible();

  await chooser.getByRole("button", { name: "gallery/runtime-map-a.png" }).click();
  const lightbox = page.getByRole("dialog", { name: "runtime-map-a.png (1 of 2)" });
  await expect(lightbox).toBeVisible();
  await expect(
    lightbox
      .getByTitle("Hold Ctrl and scroll to zoom this image")
      .locator('img[src*="profile=mobile_viewer"]')
  ).toBeVisible();
  await expect(lightbox).toContainText("Captured");
});
