import { expect, test } from "@playwright/test";

test("client-ui runtime loads configured gallery map styles through the client proxy", async ({ page }) => {
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
  expect(pageErrors).toEqual([]);
});

test("client-ui runtime keeps a gallery cluster chooser visible", async ({ page }) => {
  await page.goto("/");
  await page.getByText("Gallery", { exact: true }).click();
  await page.getByRole("button", { name: "Map" }).click();

  const cluster = page.getByRole("button", { name: "Open map cluster with 2 items" });
  await expect(cluster).toBeVisible();
  await cluster.click();

  const chooser = page.getByRole("dialog", { name: "2 items in map cluster" });
  await expect(chooser).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-a.png" })).toBeVisible();
  await expect(chooser.getByRole("button", { name: "gallery/runtime-map-b.png" })).toBeVisible();
});
