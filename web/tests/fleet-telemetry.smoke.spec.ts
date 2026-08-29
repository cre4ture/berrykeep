import { expect, test } from "@playwright/test";

const dashboard = {
  schema_version: 1,
  generated_at_unix: 1_785_888_000,
  software_version: "1.0.46",
  k_anonymity_min: 5,
  total_subjects: 12,
  by_country: [
    { country_code: "CH", subject_count: 7 },
    { country_code: "DE", subject_count: 5 }
  ],
  by_hardware_profile: [
    {
      hardware_profile_id: "85b22e9027fafc77961c9602662e75071e33ca16f9af8e47a1e41be5809f53dc",
      subject_count: 7
    },
    {
      hardware_profile_id: "c709c18dd9f1069f6842220e1c499023849fa4361a3fb44eeb1ed5cb1515a24a",
      subject_count: 5
    }
  ]
};

test("public fleet telemetry dashboard renders only aggregate statistics", async ({ page }) => {
  let dashboardRequests = 0;
  await page.route("**/v1/stats/dashboard", async (route) => {
    dashboardRequests += 1;
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(dashboard)
    });
  });

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Fleet reliability" })).toBeVisible();
  await expect(page.getByText("Privacy-preserving by design")).toBeVisible();
  await expect(page.getByText("Participants recorded")).toBeVisible();
  await expect(page.getByText("12", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("Published countries")).toBeVisible();
  await expect(page.getByText("Published hardware profiles")).toBeVisible();
  await expect(page.getByText("Participation by country")).toBeVisible();
  await expect(page.getByText("Participation by hardware profile")).toBeVisible();
  await expect(page.getByRole("cell", { name: "CH", exact: true })).toBeVisible();
  await expect(page.getByRole("cell", { name: "DE", exact: true })).toBeVisible();
  await expect(page.getByText("Schema v1")).toBeVisible();
  await expect(page.getByText(/telemetry subject IDs are shown here/)).toBeVisible();
  await expect(page.getByText("85b22e9027fafc77961c9602662e75071e33ca16f9af8e47a1e41be5809f53dc")).toHaveCount(0);

  await page.getByRole("button", { name: "Refresh" }).click();
  await expect.poll(() => dashboardRequests).toBeGreaterThanOrEqual(2);
});
